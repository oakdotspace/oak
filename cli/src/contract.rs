//! The machine contract: what a failure means to a program.
//!
//! [`classify`] is the ONE place an [`OakError`] is turned into the four
//! things automation keys off — the `error.code`, the process exit code,
//! whether retrying unchanged can help, and the exact next invocations. The
//! process exit code (`main`), the `--json` error envelope
//! ([`crate::output::JsonErrorEnvelope`]) and every document that embeds an
//! error all read it; nothing else may decide any of the four.
//!
//! The match is exhaustive with no wildcard arm, and [`ErrorCode`] has no
//! generic member, so a new `OakError` variant does not compile until it is
//! given a specific code, exit class and advice here.
//!
//! # Extending
//!
//! * A new failure: add the `OakError` variant, add its [`ErrorCode`] (one
//!   table in `core/src/error.rs`), add one arm below. Nothing else.
//! * Remote failures (workstream F2): replace the `Http | Server | R2` arm
//!   with arms over the classified failure (`connect_failed`, `rate_limited`,
//!   ...). `RemoteError` stays only for an unclassified server failure.
//! * Operation-state refusals (workstream F4): one arm per refusal; the
//!   state-dependent advice arrives on the error and is passed through
//!   [`NextCommand::Carried`] until those errors carry typed commands.

use std::path::Path;

use oak_core::{ErrorCode, ExitCode, OakError};

use crate::output::shell_quote;

/// [`NextCommand::MountListJson`] as a constant, for the mount module's
/// static recovery lists (`&[&str]`). Read-only and valid from anywhere.
pub const MOUNT_LIST_JSON: &str = "oak mount list --json";

/// One recommended invocation, typed. Rendered to the exact command line by
/// [`NextCommand::render`] and nowhere else, so a recommendation can only
/// name a command through a constructor — and the contract tests parse every
/// rendering with the real clap definition (`Cli::try_parse_from`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextCommand {
    /// `oak init`
    Init,
    /// `oak clone <repo>` (placeholder: the caller supplies the repository).
    ClonePlaceholder,
    /// `oak status --json`
    StatusJson,
    /// `oak status --json --compact`
    StatusJsonCompact,
    /// `oak info --json`
    InfoJson,
    /// `oak agent state --json`
    AgentStateJson,
    /// `oak agent state --json --compact`
    AgentStateJsonCompact,
    /// `oak merge --continue`
    MergeContinue,
    /// `oak merge --abort`
    MergeAbort,
    /// `oak conflict status --json`
    ConflictStatusJson,
    /// `oak conflict show --json`
    ConflictShowJson,
    /// `oak commit`
    Commit,
    /// `oak pull`
    Pull,
    /// `oak pull --force`
    PullForce,
    /// `oak pull --continue`
    PullContinue,
    /// `oak pull --abort`
    PullAbort,
    /// `oak fetch`
    Fetch,
    /// `oak change capture --json`
    ChangeCaptureJson,
    /// `oak switch -c <name>` (placeholder: the caller supplies the name).
    SwitchCreatePlaceholder,
    /// `oak push --repo <org>/<repo>` (placeholder).
    PushRepoPlaceholder,
    /// `oak push --remote=REMOTE`
    PushToRemote { remote: String },
    /// `oak login --remote=REMOTE`
    Login { remote: String },
    /// `oak whoami --remote=REMOTE`
    Whoami { remote: String },
    /// `oak repo list --json`
    RepoListJson,
    /// `oak branch list --json`
    BranchListJson,
    /// `oak branch review BRANCH --remote --merge-preview --json`
    BranchReviewRemoteMergePreview { branch: String },
    /// `oak branch show NAME --remote --json`
    BranchShowRemote { name: String },
    /// `oak branch diff NAME --remote --json [--against B] [--diff-mode M] [-- PATHS]`
    BranchDiffRemote {
        name: String,
        /// `None` for the default (`main`).
        against: Option<String>,
        /// `None` for the default (`tree`).
        diff_mode: Option<String>,
        paths: Vec<String>,
    },
    /// `oak branch review NAME --remote --json [--merge-preview]`
    BranchReviewRemote { name: String, merge_preview: bool },
    /// `oak sparse list --json`
    SparseListJson,
    /// `oak mount list --json`
    MountListJson,
    /// The exact command line this process was invoked with ("try the same
    /// thing again"), already shell-quoted with credentials redacted.
    SameInvocation(String),
    /// [`NextCommand::SameInvocation`] with `--clean` added (see
    /// [`crate::output::invoked_command_with_flag`]): the same branch
    /// creation, discarding uncommitted changes.
    SameInvocationClean(String),
    /// A command line that was rendered where the failure was raised and
    /// carried here on the error (`OakError` fields such as
    /// `recommended_next_commands`, `retry_command`). The escape hatch for
    /// advice that has not been converted to a typed constructor yet; later
    /// workstreams shrink its use.
    Carried(String),
}

oak_core::closed_vocabulary! {
    /// One member per [`NextCommand`] constructor, so the contract tests can
    /// prove every constructor's rendering parses. [`NextCommand::kind`] is
    /// exhaustive: a new constructor does not compile without its kind.
    pub enum NextCommandKind {
        Init => "init",
        ClonePlaceholder => "clone_placeholder",
        StatusJson => "status_json",
        StatusJsonCompact => "status_json_compact",
        InfoJson => "info_json",
        AgentStateJson => "agent_state_json",
        AgentStateJsonCompact => "agent_state_json_compact",
        MergeContinue => "merge_continue",
        MergeAbort => "merge_abort",
        ConflictStatusJson => "conflict_status_json",
        ConflictShowJson => "conflict_show_json",
        Commit => "commit",
        Pull => "pull",
        PullForce => "pull_force",
        PullContinue => "pull_continue",
        PullAbort => "pull_abort",
        Fetch => "fetch",
        ChangeCaptureJson => "change_capture_json",
        SwitchCreatePlaceholder => "switch_create_placeholder",
        PushRepoPlaceholder => "push_repo_placeholder",
        PushToRemote => "push_to_remote",
        Login => "login",
        Whoami => "whoami",
        RepoListJson => "repo_list_json",
        BranchListJson => "branch_list_json",
        BranchReviewRemoteMergePreview => "branch_review_remote_merge_preview",
        BranchShowRemote => "branch_show_remote",
        BranchDiffRemote => "branch_diff_remote",
        BranchReviewRemote => "branch_review_remote",
        SparseListJson => "sparse_list_json",
        MountListJson => "mount_list_json",
        SameInvocation => "same_invocation",
        SameInvocationClean => "same_invocation_clean",
        Carried => "carried",
    }
}

impl NextCommand {
    pub fn kind(&self) -> NextCommandKind {
        match self {
            NextCommand::Init => NextCommandKind::Init,
            NextCommand::ClonePlaceholder => NextCommandKind::ClonePlaceholder,
            NextCommand::StatusJson => NextCommandKind::StatusJson,
            NextCommand::StatusJsonCompact => NextCommandKind::StatusJsonCompact,
            NextCommand::InfoJson => NextCommandKind::InfoJson,
            NextCommand::AgentStateJson => NextCommandKind::AgentStateJson,
            NextCommand::AgentStateJsonCompact => NextCommandKind::AgentStateJsonCompact,
            NextCommand::MergeContinue => NextCommandKind::MergeContinue,
            NextCommand::MergeAbort => NextCommandKind::MergeAbort,
            NextCommand::ConflictStatusJson => NextCommandKind::ConflictStatusJson,
            NextCommand::ConflictShowJson => NextCommandKind::ConflictShowJson,
            NextCommand::Commit => NextCommandKind::Commit,
            NextCommand::Pull => NextCommandKind::Pull,
            NextCommand::PullForce => NextCommandKind::PullForce,
            NextCommand::PullContinue => NextCommandKind::PullContinue,
            NextCommand::PullAbort => NextCommandKind::PullAbort,
            NextCommand::Fetch => NextCommandKind::Fetch,
            NextCommand::ChangeCaptureJson => NextCommandKind::ChangeCaptureJson,
            NextCommand::SwitchCreatePlaceholder => NextCommandKind::SwitchCreatePlaceholder,
            NextCommand::PushRepoPlaceholder => NextCommandKind::PushRepoPlaceholder,
            NextCommand::PushToRemote { .. } => NextCommandKind::PushToRemote,
            NextCommand::Login { .. } => NextCommandKind::Login,
            NextCommand::Whoami { .. } => NextCommandKind::Whoami,
            NextCommand::RepoListJson => NextCommandKind::RepoListJson,
            NextCommand::BranchListJson => NextCommandKind::BranchListJson,
            NextCommand::BranchReviewRemoteMergePreview { .. } => {
                NextCommandKind::BranchReviewRemoteMergePreview
            }
            NextCommand::BranchShowRemote { .. } => NextCommandKind::BranchShowRemote,
            NextCommand::BranchDiffRemote { .. } => NextCommandKind::BranchDiffRemote,
            NextCommand::BranchReviewRemote { .. } => NextCommandKind::BranchReviewRemote,
            NextCommand::SparseListJson => NextCommandKind::SparseListJson,
            NextCommand::MountListJson => NextCommandKind::MountListJson,
            NextCommand::SameInvocation(_) => NextCommandKind::SameInvocation,
            NextCommand::SameInvocationClean(_) => NextCommandKind::SameInvocationClean,
            NextCommand::Carried(_) => NextCommandKind::Carried,
        }
    }

    /// One value of every constructor, with awkward arguments (spaces,
    /// quotes, a leading dash) where a constructor embeds one.
    #[doc(hidden)]
    pub fn samples() -> Vec<NextCommand> {
        let awkward = || "it's a -weird value".to_string();
        vec![
            NextCommand::Init,
            NextCommand::ClonePlaceholder,
            NextCommand::StatusJson,
            NextCommand::StatusJsonCompact,
            NextCommand::InfoJson,
            NextCommand::AgentStateJson,
            NextCommand::AgentStateJsonCompact,
            NextCommand::MergeContinue,
            NextCommand::MergeAbort,
            NextCommand::ConflictStatusJson,
            NextCommand::ConflictShowJson,
            NextCommand::Commit,
            NextCommand::Pull,
            NextCommand::PullForce,
            NextCommand::PullContinue,
            NextCommand::PullAbort,
            NextCommand::Fetch,
            NextCommand::ChangeCaptureJson,
            NextCommand::SwitchCreatePlaceholder,
            NextCommand::PushRepoPlaceholder,
            NextCommand::PushToRemote { remote: awkward() },
            NextCommand::Login { remote: awkward() },
            NextCommand::Whoami { remote: awkward() },
            NextCommand::RepoListJson,
            NextCommand::BranchListJson,
            NextCommand::BranchReviewRemoteMergePreview { branch: awkward() },
            NextCommand::BranchReviewRemoteMergePreview {
                branch: "--json".to_string(),
            },
            NextCommand::BranchShowRemote {
                name: "feature/x".to_string(),
            },
            NextCommand::BranchDiffRemote {
                name: "feature/x".to_string(),
                against: Some("other".to_string()),
                diff_mode: Some("contribution".to_string()),
                paths: vec!["src/a b.rs".to_string()],
            },
            NextCommand::BranchDiffRemote {
                name: "feature/x".to_string(),
                against: None,
                diff_mode: None,
                paths: Vec::new(),
            },
            NextCommand::BranchReviewRemote {
                name: "feature/x".to_string(),
                merge_preview: true,
            },
            NextCommand::SparseListJson,
            NextCommand::MountListJson,
            NextCommand::SameInvocation("oak merge --dry-run --json".to_string()),
            NextCommand::SameInvocationClean("oak switch -c work --clean".to_string()),
            NextCommand::Carried("oak status --json".to_string()),
        ]
    }

    /// The exact invocation, shell-quoted where it embeds a value.
    pub fn render(&self) -> String {
        match self {
            NextCommand::Init => "oak init".to_string(),
            NextCommand::ClonePlaceholder => "oak clone <repo>".to_string(),
            NextCommand::StatusJson => "oak status --json".to_string(),
            NextCommand::StatusJsonCompact => "oak status --json --compact".to_string(),
            NextCommand::InfoJson => "oak info --json".to_string(),
            NextCommand::AgentStateJson => "oak agent state --json".to_string(),
            NextCommand::AgentStateJsonCompact => "oak agent state --json --compact".to_string(),
            NextCommand::MergeContinue => "oak merge --continue".to_string(),
            NextCommand::MergeAbort => "oak merge --abort".to_string(),
            NextCommand::ConflictStatusJson => "oak conflict status --json".to_string(),
            NextCommand::ConflictShowJson => "oak conflict show --json".to_string(),
            NextCommand::Commit => "oak commit".to_string(),
            NextCommand::Pull => "oak pull".to_string(),
            NextCommand::PullForce => "oak pull --force".to_string(),
            NextCommand::PullContinue => "oak pull --continue".to_string(),
            NextCommand::PullAbort => "oak pull --abort".to_string(),
            NextCommand::Fetch => "oak fetch".to_string(),
            NextCommand::ChangeCaptureJson => "oak change capture --json".to_string(),
            NextCommand::SwitchCreatePlaceholder => "oak switch -c <name>".to_string(),
            NextCommand::PushRepoPlaceholder => {
                crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string()
            }
            NextCommand::PushToRemote { remote } => {
                format!("oak push --remote={}", shell_quote(remote))
            }
            NextCommand::Login { remote } => format!("oak login --remote={}", shell_quote(remote)),
            NextCommand::Whoami { remote } => {
                format!("oak whoami --remote={}", shell_quote(remote))
            }
            NextCommand::RepoListJson => "oak repo list --json".to_string(),
            NextCommand::BranchListJson => "oak branch list --json".to_string(),
            NextCommand::BranchReviewRemoteMergePreview { branch } => format!(
                "oak branch review --remote --merge-preview --json -- {}",
                shell_quote(branch)
            ),
            NextCommand::BranchShowRemote { name } => {
                format!("oak branch show {} --remote --json", shell_quote(name))
            }
            NextCommand::BranchDiffRemote {
                name,
                against,
                diff_mode,
                paths,
            } => {
                let mut line = format!("oak branch diff {} --remote --json", shell_quote(name));
                if let Some(against) = against {
                    line.push_str(&format!(" --against {}", shell_quote(against)));
                }
                if let Some(diff_mode) = diff_mode {
                    line.push_str(&format!(" --diff-mode {}", shell_quote(diff_mode)));
                }
                if !paths.is_empty() {
                    line.push_str(" --");
                    for path in paths {
                        line.push_str(&format!(" {}", shell_quote(path)));
                    }
                }
                line
            }
            NextCommand::BranchReviewRemote {
                name,
                merge_preview,
            } => {
                let mut line = format!("oak branch review {} --remote --json", shell_quote(name));
                if *merge_preview {
                    line.push_str(" --merge-preview");
                }
                line
            }
            NextCommand::SparseListJson => "oak sparse list --json".to_string(),
            NextCommand::MountListJson => MOUNT_LIST_JSON.to_string(),
            NextCommand::SameInvocation(command)
            | NextCommand::SameInvocationClean(command)
            | NextCommand::Carried(command) => command.clone(),
        }
    }

    /// Render a list, dropping exact duplicates while keeping order.
    pub fn render_all(commands: &[NextCommand]) -> Vec<String> {
        let mut rendered: Vec<String> = Vec::with_capacity(commands.len());
        for command in commands {
            let line = command.render();
            if !rendered.contains(&line) {
                rendered.push(line);
            }
        }
        rendered
    }

    fn carried(commands: &[String]) -> Vec<NextCommand> {
        commands.iter().cloned().map(NextCommand::Carried).collect()
    }
}

/// Split a rendered command line the way a POSIX shell would for the subset
/// [`shell_quote`] produces (bare words and single-quoted words with the
/// `'\''` escape). `None` when the line is not well-formed. Used by the
/// contract tests to feed renderings back through clap.
pub fn split_command_line(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => current.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => current.push(chars.next()?),
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                current.push(chars.next()?);
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

/// Everything about the invocation's surroundings that classification
/// needs, gathered BEFORE classifying. [`classify_in`] itself performs no
/// I/O and reads no process state: state-dependent advice takes the state
/// as input.
#[derive(Clone, Debug)]
pub struct AdviceContext {
    /// Whether commands that need a checkout (`oak status`) can run where
    /// the command ran.
    pub in_checkout: bool,
    /// The exact command line of this process, shell-quoted with credentials
    /// redacted, for "run the same command again" advice. `None` when it is
    /// not known (library callers).
    pub invoked: Option<String>,
    /// For a local `oak branch show|diff|review NAME` in a checkout that has
    /// a remote: NAME and the `--remote` form of the same command, offered
    /// when NAME turns out not to exist locally (fb-83).
    pub remote_retry: Option<RemoteRetry>,
}

/// See [`AdviceContext::remote_retry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRetry {
    pub branch: String,
    pub command: NextCommand,
}

impl AdviceContext {
    /// Nothing is known about the surroundings (library callers, tests). A
    /// checkout is assumed: that is where almost every failing command runs.
    pub fn unknown() -> Self {
        Self {
            in_checkout: true,
            invoked: None,
            remote_retry: None,
        }
    }

    /// The context of this process, running in `cwd`. This is the one place
    /// the surroundings are observed (a repository lookup and argv).
    pub fn for_process(cwd: Option<&Path>) -> Self {
        Self {
            in_checkout: match cwd {
                Some(cwd) => crate::resolve::resolve(cwd).is_ok(),
                None => true,
            },
            invoked: crate::output::invoked_command(),
            remote_retry: None,
        }
    }

    pub fn with_remote_retry(mut self, remote_retry: Option<RemoteRetry>) -> Self {
        self.remote_retry = remote_retry;
        self
    }
}

/// The machine meaning of one failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorContract {
    pub code: ErrorCode,
    pub exit: ExitCode,
    /// `Some(true)`: running the same command again, unchanged, may succeed.
    /// `Some(false)`: it cannot until something else changes. `None`: not
    /// known (the envelope omits the field). Only ever `Some` where the
    /// answer follows from what the failure IS — never a guess.
    pub retryable: Option<bool>,
    pub next: Vec<NextCommand>,
}

impl ErrorContract {
    fn new(code: ErrorCode, exit: ExitCode) -> Self {
        Self {
            code,
            exit,
            retryable: None,
            next: Vec::new(),
        }
    }

    fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = Some(retryable);
        self
    }

    fn retryable_if_known(mut self, retryable: Option<bool>) -> Self {
        self.retryable = retryable;
        self
    }

    fn next(mut self, next: impl IntoIterator<Item = NextCommand>) -> Self {
        self.next = next.into_iter().collect();
        self
    }

    pub fn rendered_next_commands(&self) -> Vec<String> {
        NextCommand::render_all(&self.next)
    }
}

/// [`classify_in`] with nothing known about the surroundings.
pub fn classify(err: &OakError) -> ErrorContract {
    classify_in(err, &AdviceContext::unknown())
}

/// The one exhaustive `OakError` → contract mapping. No wildcard arm, no
/// string matching, no I/O: see the module docs before adding any.
pub fn classify_in(err: &OakError, context: &AdviceContext) -> ErrorContract {
    use ErrorCode as Code;
    use ExitCode as Exit;
    use NextCommand as Next;

    let contract = ErrorContract::new;
    let repair_history = || [Next::Fetch, Next::PullForce, Next::StatusJson];
    let in_checkout = |commands: Vec<Next>| {
        if context.in_checkout {
            commands
        } else {
            Vec::new()
        }
    };
    let same_invocation = || context.invoked.clone().map(Next::SameInvocation);

    match err {
        // ---- usage ------------------------------------------------------
        // The parser rejected the command line: definitionally not
        // retryable unchanged.
        OakError::Usage(_) => contract(Code::InvalidArgument, Exit::Usage).retryable(false),
        // Raised for many things besides bad arguments — including server
        // refusals that say "retry" — so nothing is claimed about retrying.
        OakError::InvalidArgument(_) | OakError::InvalidPath(_) => {
            contract(Code::InvalidArgument, Exit::Usage)
        }
        OakError::DetachedHeadCommit(_) => {
            contract(Code::InvalidArgument, Exit::Usage).next([Next::SwitchCreatePlaceholder])
        }
        OakError::InvalidHash(_) => contract(Code::InvalidHash, Exit::Failure),
        OakError::Unsupported { .. } | OakError::UnsupportedChangeCaptureFormat(_) => {
            contract(Code::Unsupported, Exit::Failure)
        }
        OakError::Config(_) => {
            contract(Code::ConfigurationError, Exit::Failure).next([Next::InfoJson])
        }
        OakError::RepoNotLinked(_) | OakError::RemoteNotConfigured => {
            contract(Code::ConfigurationError, Exit::Failure)
                .next([Next::PushRepoPlaceholder, Next::InfoJson])
        }

        // ---- local repository state --------------------------------------
        OakError::RepoNotFound => {
            contract(Code::RepoNotFound, Exit::Failure).next([Next::Init, Next::ClonePlaceholder])
        }
        OakError::RepoAlreadyExists => {
            contract(Code::RepoAlreadyExists, Exit::Failure).next([Next::StatusJson])
        }
        OakError::RepoLocked => contract(Code::RepoLocked, Exit::Locked)
            .retryable(true)
            .next([Next::StatusJson]),
        OakError::NoCommits => contract(Code::NoCommits, Exit::Failure).next([Next::StatusJson]),
        OakError::NoEdits => contract(Code::NoEdits, Exit::Failure).next([Next::StatusJson]),
        OakError::BranchNotFound(missing) => {
            // fb-83: a branch missing locally may exist only on the remote;
            // lead with the exact `--remote` form of the same command.
            let remote_form = context
                .remote_retry
                .as_ref()
                .filter(|retry| retry.branch == *missing)
                .map(|retry| retry.command.clone());
            contract(Code::BranchNotFound, Exit::Failure).next(
                remote_form
                    .into_iter()
                    .chain(in_checkout(vec![Next::BranchListJson])),
            )
        }
        OakError::BranchAlreadyExists(_) => {
            contract(Code::BranchAlreadyExists, Exit::Failure).next([Next::BranchListJson])
        }
        OakError::BranchClosed(_) => {
            contract(Code::BranchClosed, Exit::Failure).next([Next::BranchListJson])
        }
        OakError::TrunkIsServerOnly(_) => {
            contract(Code::BranchIsServerOnly, Exit::Failure).next([Next::BranchListJson])
        }
        OakError::CommitNotFound(_) => contract(Code::CommitNotFound, Exit::Failure),
        OakError::BlobNotFound(_) => contract(Code::BlobNotFound, Exit::Failure),
        OakError::ManifestNotFound(_) => contract(Code::ManifestNotFound, Exit::Failure),
        OakError::GroupNotFound(_) => contract(Code::GroupNotFound, Exit::Failure),
        OakError::RestrictedContent(_) => contract(Code::RestrictedContent, Exit::Failure),

        // ---- local machine -----------------------------------------------
        OakError::DiskFull { .. } => contract(Code::DiskFull, Exit::Failure),
        OakError::Io(error) if error.kind() == std::io::ErrorKind::StorageFull => {
            contract(Code::DiskFull, Exit::Failure)
        }
        OakError::Io(_) => contract(Code::IoError, Exit::Failure),
        OakError::Database(_) => contract(Code::DatabaseError, Exit::Failure),
        OakError::Serialization(_) => contract(Code::SerializationError, Exit::Failure),
        OakError::Git(_) => contract(Code::GitError, Exit::Failure),

        // ---- working tree --------------------------------------------------
        OakError::UncommittedChanges | OakError::DirtyWorkingTree(_) => {
            contract(Code::DirtyWorktree, Exit::WorktreeBlocked).next([Next::StatusJson])
        }
        OakError::InterruptedWorktreeUpdate { next_commands, .. } => {
            contract(Code::InterruptedWorktreeUpdate, Exit::WorktreeBlocked)
                .next(Next::carried(next_commands))
        }
        // What resolves it depends on the transition (see
        // `local_changes_message`, which states the same commands): a pull
        // merges committed work, so commit-then-pull surfaces a conflict; a
        // new branch starts from the newer main, so the edit is either kept
        // on the current branch (commit, pull there) or discarded with
        // `--clean`. Something in the way that a commit cannot resolve has to
        // be moved by hand first: only the read-only status is offered.
        OakError::LocalChangesWouldBeOverwritten {
            transition,
            in_the_way,
            clean_command,
            ..
        } => contract(Code::LocalChangesWouldBeOverwritten, Exit::WorktreeBlocked).next(
            match (in_the_way, transition) {
                (true, _) => vec![Next::StatusJson],
                (false, oak_core::WorktreeTransition::Pull) => vec![Next::Commit, Next::Pull],
                (false, oak_core::WorktreeTransition::CreateBranch) => {
                    let mut next = vec![Next::Commit, Next::Pull];
                    next.extend(clean_command.clone().map(Next::SameInvocationClean));
                    next
                }
                (false, oak_core::WorktreeTransition::Other) => vec![Next::StatusJson],
            },
        ),
        OakError::PathOutsideView {
            recommended_next_commands,
            ..
        } => contract(Code::PathOutsideView, Exit::WorktreeBlocked)
            .next(Next::carried(recommended_next_commands)),

        // ---- change capture ------------------------------------------------
        OakError::WorktreeCaptureRaced { .. } => {
            contract(Code::WorktreeCaptureRaced, Exit::Failure)
                .retryable(true)
                .next([Next::ChangeCaptureJson])
        }
        OakError::CapturedBlobIntegrity { .. } => {
            contract(Code::CapturedBlobIntegrity, Exit::Failure)
        }
        OakError::CapturedTreeIntegrity { .. } => {
            contract(Code::CapturedTreeIntegrity, Exit::Failure)
        }
        OakError::ChangeCaptureNotFound { .. } => {
            contract(Code::ChangeCaptureNotFound, Exit::Failure).next([Next::ChangeCaptureJson])
        }
        OakError::ChangeCaptureIntegrity { .. } => {
            contract(Code::ChangeCaptureIntegrity, Exit::Failure)
        }
        OakError::ChangeCaptureScopeExcluded { .. } => {
            contract(Code::CaptureScopeExcluded, Exit::Failure)
        }

        // ---- history, merges, conflicts -------------------------------------
        // The conflict advice is not state-aware yet (it names the pull flow
        // even for a merge conflict); workstream F4 derives it from the
        // operation in progress.
        OakError::MergeConflict(_) | OakError::ConflictDetected => {
            contract(Code::ConflictDetected, Exit::Conflict).next([
                Next::ConflictStatusJson,
                Next::ConflictShowJson,
                Next::PullContinue,
                Next::PullAbort,
            ])
        }
        OakError::MergeInProgress => contract(Code::MergeInProgress, Exit::Conflict)
            .next([Next::MergeContinue, Next::MergeAbort]),
        OakError::NoMergeInProgress => {
            contract(Code::NoMergeInProgress, Exit::Failure).next([Next::ConflictStatusJson])
        }
        OakError::NoSyncInProgress => {
            contract(Code::NoSyncInProgress, Exit::Failure).next([Next::ConflictStatusJson])
        }
        OakError::MergeFailed(_) => contract(Code::MergeFailed, Exit::Failure),
        // Both merge verdicts carry the advice the assessment derived from
        // its evidence, so the document that embeds the error and the error
        // itself name one list.
        OakError::MergeInvariantViolation { next_commands, .. } => {
            contract(Code::MergeInvariantViolation, Exit::Conflict)
                .next(Next::carried(next_commands))
        }
        OakError::MergePredictionUncertified { next_commands, .. } => {
            contract(Code::MergePredictionUncertified, Exit::MergeUncertified)
                .next(Next::carried(next_commands))
        }
        OakError::LocalCommitsNotInRemoteHistory => {
            contract(Code::LocalCommitsNotInRemoteHistory, Exit::Conflict).next([Next::Pull])
        }
        OakError::RemoteCommitsNotInLocalHistory => {
            contract(Code::RemoteCommitsNotInLocalHistory, Exit::Conflict).next([Next::Pull])
        }
        OakError::IncompleteAncestry { .. } => {
            contract(Code::IncompleteAncestry, Exit::Conflict).next(repair_history())
        }
        OakError::IncompleteManifestData { .. } => {
            contract(Code::IncompleteManifestData, Exit::Conflict).next(repair_history())
        }
        OakError::IncompleteCommitData { .. } => {
            contract(Code::IncompleteCommitData, Exit::Conflict).next(repair_history())
        }
        OakError::IncompleteBlobData { .. } => {
            contract(Code::IncompleteBlobData, Exit::Conflict).next(repair_history())
        }
        OakError::NoVerifiedCommonAncestor { .. } => {
            contract(Code::NoVerifiedCommonAncestor, Exit::Conflict).next(repair_history())
        }

        // ---- remote access ---------------------------------------------------
        // Unclassified transport/server failure. F2 splits this arm by
        // classified cause; `oak status --json` is only offered where a
        // checkout exists to run it in.
        OakError::Http(_) | OakError::Server(_) | OakError::R2(_) => {
            contract(Code::RemoteError, Exit::Remote).next(in_checkout(vec![Next::StatusJson]))
        }
        OakError::RemoteRepoNotFound(_) => {
            contract(Code::RemoteRepoNotFound, Exit::Remote).next([Next::RepoListJson])
        }
        OakError::RemoteRepoAlreadyExists(_) => {
            contract(Code::RemoteRepoAlreadyExists, Exit::Remote).next([Next::RepoListJson])
        }
        OakError::RemoteMoved { origin } => {
            contract(Code::RemoteMoved, Exit::Failure).next([Next::PushToRemote {
                remote: origin.clone(),
            }])
        }
        OakError::AuthRequired { remote, status } => contract(
            if *status == 403 {
                Code::AuthDenied
            } else {
                Code::AuthRequired
            },
            Exit::Remote,
        )
        .next([
            Next::Login {
                remote: remote.clone(),
            },
            Next::Whoami {
                remote: remote.clone(),
            },
        ]),
        // No credential is stored; nothing was sent. Same class as a 401.
        OakError::NotLoggedIn { remote } => {
            contract(Code::AuthRequired, Exit::Remote).next([Next::Login {
                remote: remote.clone(),
            }])
        }
        OakError::PublicationUnconfirmed {
            reconciliation_commands,
            ..
        } => contract(Code::PublicationUnconfirmed, Exit::Remote)
            .next(Next::carried(reconciliation_commands)),
        OakError::PushCapabilityUnavailable {
            recommended_next_commands,
            ..
        } => contract(Code::PushCapabilityUnavailable, Exit::Remote)
            .next(Next::carried(recommended_next_commands)),
        OakError::IntegrityInconclusive {
            recommended_next_commands,
            ..
        } => contract(Code::IntegrityInconclusive, Exit::IntegrityInconclusive)
            .next(Next::carried(recommended_next_commands)),
        OakError::CloneAdmissionTokenTooLarge {
            recommended_next_commands,
            ..
        } => contract(Code::CloneAdmissionTokenTooLarge, Exit::Remote)
            .next(Next::carried(recommended_next_commands)),
        OakError::IntegrityAdmissionBusy(_) => contract(Code::IntegrityAdmissionBusy, Exit::Remote)
            .retryable(true)
            .next(same_invocation()),

        // ---- multi-phase commands ----------------------------------------------
        OakError::FinishPreflight(details) => contract(
            Code::FinishPreflightFailed,
            finish_blocker_exit(details.blocker),
        )
        .next(saga_next_commands(
            details.retry_command.as_deref(),
            &details.manual_recovery_commands,
            true,
        )),
        OakError::FinishPhaseFailed(details) => contract(Code::FinishPhaseFailed, Exit::Remote)
            .next(saga_next_commands(
                details.retry_command.as_deref(),
                &details.manual_recovery_commands,
                true,
            )),
        OakError::CommitPhaseFailed(details) => contract(Code::CommitPhaseFailed, Exit::Remote)
            .next(saga_next_commands(
                details.retry_command.as_deref(),
                &details.manual_recovery_commands,
                false,
            )),
        OakError::DescriptionStale {
            state,
            refresh_commands,
            ..
        } => {
            // `remote_unverified`: the remote could not be read — a
            // transport-class failure where the same command may work later.
            // Every other state needs the description reconciled first.
            let (exit, retryable) = match state {
                oak_core::DescriptionStaleState::RemoteUnverified => (Exit::Remote, true),
                oak_core::DescriptionStaleState::PendingLocalEdit
                | oak_core::DescriptionStaleState::RemoteChanged
                | oak_core::DescriptionStaleState::LocalChanged => (Exit::Conflict, false),
            };
            contract(Code::DescriptionStale, exit)
                .retryable(retryable)
                .next(Next::carried(refresh_commands))
        }

        // ---- approvals ---------------------------------------------------------
        // The code was parsed from the refusal reason where it was received
        // (`ErrorCode::from_approval_reason`). Waiting never clears a
        // refusal, so it is not retryable unchanged.
        OakError::ApprovalRefused {
            code,
            recommended_next_commands,
            ..
        } => contract(*code, Exit::Failure)
            .retryable(false)
            .next(Next::carried(recommended_next_commands)),

        // ---- commands with their own established exit codes ---------------------
        // `oak feedback`: a failed request exits 1 (not the remote class),
        // and repo advice is unrelated — only what the failure carries.
        OakError::FeedbackFailed {
            usage,
            code,
            retryable,
            next_commands,
            ..
        } => contract(*code, if *usage { Exit::Usage } else { Exit::Failure })
            .retryable_if_known(*retryable)
            .next(Next::carried(next_commands)),
        // `oak root pull|push` refused before changing anything.
        OakError::RootRefused {
            reason,
            next_commands,
            ..
        } => contract(
            match reason {
                oak_core::RootRefusal::Conflict => Code::RootConflict,
                oak_core::RootRefusal::PullRequired => Code::PullRequired,
                oak_core::RootRefusal::HeadMoved => Code::HeadMoved,
            },
            Exit::Conflict,
        )
        .next(Next::carried(next_commands)),
    }
}

/// The exit class of each `oak finish` preflight blocker. Exhaustive.
fn finish_blocker_exit(blocker: oak_core::FinishBlocker) -> ExitCode {
    use oak_core::FinishBlocker as Blocker;
    match blocker {
        Blocker::InvalidDescription => ExitCode::Usage,
        Blocker::AuthMissing
        | Blocker::AuthFailed
        | Blocker::RemoteUnreachable
        | Blocker::RemoteRepoMissing => ExitCode::Remote,
        Blocker::MergeInProgress
        | Blocker::SyncInProgress
        | Blocker::NoCurrentBranch
        | Blocker::BranchNotFound
        | Blocker::BranchClosed
        | Blocker::RemoteNotConfigured
        | Blocker::RemoteIdentityMissing => ExitCode::Failure,
    }
}

/// The commands a failed multi-phase command (finish, commit --push) carries:
/// its retry command first, then its manual recovery commands, without
/// duplicates. `finish` falls back to the agent state document when it
/// carries none; `commit` keeps its recovery list's own order when that list
/// already names the retry.
fn saga_next_commands(
    retry_command: Option<&str>,
    manual_recovery_commands: &[String],
    finish: bool,
) -> Vec<NextCommand> {
    let mut commands: Vec<NextCommand> = Vec::new();
    let retry_listed =
        retry_command.is_some_and(|retry| manual_recovery_commands.iter().any(|cmd| cmd == retry));
    let retry_first = retry_command.filter(|_| finish || !retry_listed);
    for command in retry_first
        .into_iter()
        .map(str::to_string)
        .chain(manual_recovery_commands.iter().cloned())
    {
        let command = NextCommand::Carried(command);
        if !commands.contains(&command) {
            commands.push(command);
        }
    }
    if commands.is_empty() && finish {
        commands.push(NextCommand::AgentStateJson);
    }
    commands
}

/// One sample (or more, where a variant has several contract outcomes) of
/// every `OakError` variant, for tests that must hold for all of them.
/// `every_variant_has_a_sample` proves the coverage.
#[doc(hidden)]
pub fn sample_errors() -> Vec<OakError> {
    use oak_core::{CommitPhaseError, FinishPhaseError, FinishPreflightError};
    let s = |text: &str| text.to_string();
    let finish_preflight = |blocker: oak_core::FinishBlocker, retry: Option<&str>| {
        OakError::FinishPreflight(Box::new(FinishPreflightError {
            blocker,
            message: s("finish refused"),
            pending_phases: Vec::new(),
            retry_command: retry.map(str::to_string),
            manual_recovery_commands: vec![s("oak status --json")],
        }))
    };
    vec![
        OakError::DiskFull { path: s("/x") },
        OakError::RepoNotFound,
        OakError::RepoAlreadyExists,
        OakError::NoCommits,
        OakError::InvalidHash(s("zz")),
        OakError::InvalidPath(s("bad path")),
        OakError::BlobNotFound(s("abc")),
        OakError::CommitNotFound(s("abc")),
        OakError::BranchNotFound(s("nope")),
        OakError::BranchAlreadyExists(s("dup")),
        OakError::BranchClosed(s("old")),
        OakError::NoEdits,
        OakError::ManifestNotFound(s("abc")),
        OakError::UncommittedChanges,
        OakError::WorktreeCaptureRaced { detail: s("a.txt") },
        OakError::UnsupportedChangeCaptureFormat(s("sha1")),
        OakError::ChangeCaptureScopeExcluded {
            path: s("a"),
            reason: s("ignored"),
            remedy: s("unignore it"),
        },
        OakError::CapturedBlobIntegrity {
            path: s("a"),
            hash: s("abc"),
            detail: s("mismatch"),
        },
        OakError::CapturedTreeIntegrity {
            hash: s("abc"),
            detail: s("mismatch"),
        },
        OakError::ChangeCaptureNotFound {
            capture_id: s("c1"),
        },
        OakError::ChangeCaptureIntegrity {
            capture_id: s("c1"),
            detail: s("mismatch"),
        },
        OakError::DirtyWorkingTree(s("dirty")),
        OakError::ConflictDetected,
        OakError::LocalCommitsNotInRemoteHistory,
        OakError::RemoteCommitsNotInLocalHistory,
        OakError::RemoteNotConfigured,
        OakError::RemoteMoved {
            origin: s("https://new.example/with space"),
        },
        OakError::RemoteRepoNotFound(s("acme/app")),
        OakError::RemoteRepoAlreadyExists(s("acme/app")),
        OakError::MergeFailed(s("boom")),
        OakError::IncompleteAncestry {
            left: s("a"),
            right: s("b"),
            missing: s("c"),
        },
        OakError::IncompleteManifestData {
            left: s("a"),
            right: s("b"),
            missing: s("c"),
        },
        OakError::IncompleteCommitData {
            context: s("log"),
            missing: s("c"),
        },
        OakError::IncompleteBlobData {
            context: s("cat"),
            missing: s("c"),
        },
        OakError::NoVerifiedCommonAncestor {
            left: s("a"),
            right: s("b"),
        },
        OakError::MergeConflict(2),
        OakError::MergeInvariantViolation {
            detail: s("a.txt"),
            next_commands: vec![s("oak branch review feature --merge-preview --json")],
        },
        OakError::MergePredictionUncertified {
            detail: s("stale target"),
            next_commands: vec![s("oak fetch"), s("oak merge --dry-run --json")],
        },
        OakError::IntegrityInconclusive {
            detail: s("budget"),
            recommended_next_commands: vec![s("oak clone --shallow acme/app")],
        },
        OakError::CloneAdmissionTokenTooLarge {
            detail: s("too large"),
            recommended_next_commands: vec![s("oak clone --shallow 'acme/app' 'dest dir'")],
        },
        OakError::IntegrityAdmissionBusy(s("busy")),
        OakError::MergeInProgress,
        OakError::InterruptedWorktreeUpdate {
            message: s("interrupted"),
            next_commands: vec![s("oak pull --abort"), s("oak pull")],
        },
        OakError::LocalChangesWouldBeOverwritten {
            command: s("oak switch -c work"),
            clean_command: Some(s("oak switch -c work --clean")),
            transition: oak_core::WorktreeTransition::CreateBranch,
            paths: vec![(s("a.txt"), s("modified locally"))],
            total: 1,
            in_the_way: false,
        },
        OakError::LocalChangesWouldBeOverwritten {
            command: s("oak pull --json"),
            clean_command: None,
            transition: oak_core::WorktreeTransition::Pull,
            paths: vec![(s("a.txt"), s("modified locally"))],
            total: 3,
            in_the_way: false,
        },
        OakError::LocalChangesWouldBeOverwritten {
            command: s("oak merge"),
            clean_command: None,
            transition: oak_core::WorktreeTransition::Other,
            paths: vec![(s("build"), s("in the way"))],
            total: 1,
            in_the_way: true,
        },
        OakError::NoMergeInProgress,
        OakError::Io(std::io::Error::other("io")),
        OakError::Io(std::io::Error::new(std::io::ErrorKind::StorageFull, "full")),
        OakError::Database(s("db")),
        OakError::Serialization(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
        OakError::Http(s("connection refused")),
        OakError::Server(s("HTTP 500")),
        OakError::PublicationUnconfirmed {
            operation: s("push"),
            reason: s("timeout"),
            reconciliation_commands: vec![s("oak agent state --refresh --json --compact")],
        },
        OakError::AuthRequired {
            remote: s("https://oak.example"),
            status: 401,
        },
        OakError::AuthRequired {
            remote: s("https://oak.example"),
            status: 403,
        },
        OakError::R2(s("r2")),
        OakError::Git(s("git")),
        OakError::Config(s("bad config value")),
        OakError::RepoNotLinked(s("not linked; run `oak push --repo <org>/<repo>`")),
        OakError::Usage(s("unexpected argument '--bogus' found")),
        OakError::DetachedHeadCommit(s("HEAD is detached")),
        OakError::NoSyncInProgress,
        OakError::TrunkIsServerOnly(s("`main` only exists on the server")),
        OakError::NotLoggedIn {
            remote: s("https://oak.example"),
        },
        OakError::FeedbackFailed {
            message: s("could not reach https://oak.example"),
            usage: false,
            code: ErrorCode::RemoteError,
            retryable: None,
            next_commands: vec![s("oak feedback show fb-1 --json")],
            transition: None,
        },
        OakError::RootRefused {
            reason: oak_core::RootRefusal::Conflict,
            message: s("2 file(s) changed both here and on the root"),
            paths: vec![s("a.txt")],
            next_commands: vec![s("oak root pull --force")],
        },
        OakError::RootRefused {
            reason: oak_core::RootRefusal::HeadMoved,
            message: s("moved"),
            paths: Vec::new(),
            next_commands: vec![s("oak root pull"), s("oak root push")],
        },
        finish_preflight(
            oak_core::FinishBlocker::InvalidDescription,
            Some("oak finish --desc-file /tmp/d.txt --json"),
        ),
        finish_preflight(oak_core::FinishBlocker::RemoteUnreachable, None),
        OakError::FinishPreflight(Box::new(FinishPreflightError {
            blocker: oak_core::FinishBlocker::SyncInProgress,
            message: s("finish refused"),
            pending_phases: Vec::new(),
            retry_command: None,
            manual_recovery_commands: Vec::new(),
        })),
        OakError::FinishPhaseFailed(Box::new(FinishPhaseError {
            phase: s("push"),
            completed_phases: vec![s("description")],
            pending_phases: vec![s("push")],
            message: s("push failed"),
            retry_command: Some(s("oak finish --json")),
            manual_recovery_commands: vec![s("oak push --json")],
        })),
        OakError::CommitPhaseFailed(Box::new(CommitPhaseError {
            phase: s("push"),
            committed: true,
            pushed: false,
            published: false,
            remote_contacted: true,
            branch: s("b"),
            head_before: None,
            head_after: None,
            unpushed_commit_count: 1,
            message: s("push failed"),
            retry_command: Some(s("oak push --json")),
            manual_recovery_commands: vec![s("oak status --json")],
        })),
        OakError::RepoLocked,
        OakError::Unsupported {
            backend: "git",
            op: "sparse",
        },
        OakError::GroupNotFound(s("web")),
        OakError::InvalidArgument(s("bad flag")),
        OakError::RestrictedContent(s("secret/")),
        OakError::DescriptionStale {
            branch: s("b"),
            state: oak_core::DescriptionStaleState::RemoteChanged,
            message: s("stale"),
            refresh_commands: vec![s("oak pull --branch-only --json")],
        },
        OakError::DescriptionStale {
            branch: s("b"),
            state: oak_core::DescriptionStaleState::RemoteUnverified,
            message: s("stale"),
            refresh_commands: vec![s("oak pull --branch-only --json")],
        },
        OakError::ApprovalRefused {
            code: ErrorCode::ApprovalsRequired,
            server_code: None,
            message: s("needs approval"),
            missing: Vec::new(),
            hidden: 0,
            recommended_next_commands: vec![s("oak owners --branch --json")],
        },
        OakError::ApprovalRefused {
            code: ErrorCode::ApprovalRefused,
            server_code: Some(s("some_future_server_reason")),
            message: s("refused"),
            missing: Vec::new(),
            hidden: 0,
            recommended_next_commands: vec![s("oak open --print")],
        },
        OakError::PushCapabilityUnavailable {
            message: s("needs staged_v1"),
            recommended_next_commands: vec![s("oak push --plan --json")],
        },
        OakError::PathOutsideView {
            paths: vec![s("other/x")],
            recommended_next_commands: vec![s("oak sparse add other")],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::OakErrorKind;
    use std::collections::HashSet;

    #[test]
    fn every_variant_has_a_sample() {
        let sampled: HashSet<OakErrorKind> = sample_errors().iter().map(OakError::kind).collect();
        for kind in OakErrorKind::ALL {
            assert!(
                sampled.contains(kind),
                "contract::sample_errors() has no sample of OakError::{kind}"
            );
        }
    }

    /// The contract invariant for every variant: a specific code, an exit
    /// code from the documented table, and never the generic class for a
    /// variant that used to have a specific one.
    #[test]
    fn every_variant_maps_to_a_specific_code_and_a_table_exit() {
        for error in sample_errors() {
            let contract = classify(&error);
            let code = contract.code.as_str();
            assert!(ErrorCode::ALL.contains(&contract.code));
            assert!(
                !matches!(code, "error" | "generic" | "unknown"),
                "{:?} maps to the generic code {code}",
                error.kind()
            );
            assert!(
                (1..=8).contains(&contract.exit.code()),
                "{:?} exits {}",
                error.kind(),
                contract.exit.code()
            );
            if contract.code == ErrorCode::RemoteError {
                assert!(
                    matches!(
                        error.kind(),
                        // `oak feedback`'s request failures are unclassified
                        // remote failures too (F2 classifies them).
                        OakErrorKind::Http
                            | OakErrorKind::Server
                            | OakErrorKind::R2
                            | OakErrorKind::FeedbackFailed
                    ),
                    "{:?} must not use the coarse remote_error code",
                    error.kind()
                );
            }
        }
    }

    /// Append-only compatibility: every code string and exit code that was
    /// already specific before the single mapping existed is unchanged.
    #[test]
    fn previously_specific_codes_and_exits_are_unchanged() {
        let expect = |error: OakError, code: &str, exit: i32| {
            let contract = classify(&error);
            assert_eq!(contract.code.as_str(), code, "{:?}", error.kind());
            assert_eq!(contract.exit.code(), exit, "{:?}", error.kind());
        };
        let s = |text: &str| text.to_string();
        expect(OakError::RepoNotFound, "repo_not_found", 1);
        expect(OakError::RepoLocked, "repo_locked", 3);
        expect(OakError::MergeInProgress, "merge_in_progress", 5);
        expect(OakError::MergeConflict(1), "conflict_detected", 5);
        expect(OakError::ConflictDetected, "conflict_detected", 5);
        expect(OakError::UncommittedChanges, "dirty_worktree", 4);
        expect(OakError::DirtyWorkingTree(s("x")), "dirty_worktree", 4);
        expect(OakError::InvalidArgument(s("x")), "invalid_argument", 2);
        expect(OakError::InvalidPath(s("x")), "invalid_argument", 2);
        expect(OakError::DiskFull { path: s("x") }, "disk_full", 1);
        expect(OakError::Config(s("x")), "configuration_error", 1);
        expect(OakError::RemoteNotConfigured, "configuration_error", 1);
        expect(OakError::Http(s("x")), "remote_error", 6);
        expect(OakError::Server(s("x")), "remote_error", 6);
        expect(OakError::R2(s("x")), "remote_error", 6);
        expect(
            OakError::IntegrityAdmissionBusy(s("x")),
            "integrity_admission_busy",
            6,
        );
        expect(
            OakError::AuthRequired {
                remote: s("r"),
                status: 401,
            },
            "auth_required",
            6,
        );
        expect(
            OakError::AuthRequired {
                remote: s("r"),
                status: 403,
            },
            "auth_denied",
            6,
        );
        for error in sample_errors() {
            let (code, exit) = match error.kind() {
                OakErrorKind::InterruptedWorktreeUpdate => ("interrupted_worktree_update", 4),
                OakErrorKind::PathOutsideView => ("path_outside_view", 4),
                OakErrorKind::IncompleteAncestry => ("incomplete_ancestry", 5),
                OakErrorKind::IncompleteManifestData => ("incomplete_manifest_data", 5),
                OakErrorKind::IncompleteCommitData => ("incomplete_commit_data", 5),
                OakErrorKind::IncompleteBlobData => ("incomplete_blob_data", 5),
                OakErrorKind::NoVerifiedCommonAncestor => ("no_verified_common_ancestor", 5),
                OakErrorKind::WorktreeCaptureRaced => ("worktree_capture_raced", 1),
                OakErrorKind::UnsupportedChangeCaptureFormat => ("unsupported", 1),
                OakErrorKind::CapturedBlobIntegrity => ("captured_blob_integrity", 1),
                OakErrorKind::CapturedTreeIntegrity => ("captured_tree_integrity", 1),
                OakErrorKind::ChangeCaptureNotFound => ("change_capture_not_found", 1),
                OakErrorKind::ChangeCaptureIntegrity => ("change_capture_integrity", 1),
                OakErrorKind::ChangeCaptureScopeExcluded => ("capture_scope_excluded", 1),
                OakErrorKind::IntegrityInconclusive => ("integrity_inconclusive", 8),
                OakErrorKind::CloneAdmissionTokenTooLarge => ("clone_admission_token_too_large", 6),
                OakErrorKind::PublicationUnconfirmed => ("publication_unconfirmed", 6),
                OakErrorKind::PushCapabilityUnavailable => ("push_capability_unavailable", 6),
                OakErrorKind::FinishPhaseFailed => ("finish_phase_failed", 6),
                OakErrorKind::CommitPhaseFailed => ("commit_phase_failed", 6),
                _ => continue,
            };
            expect(error, code, exit);
        }
        // Exit codes that were specific while the code was the catch-all.
        let exit_of = |error: OakError| classify(&error).exit.code();
        assert_eq!(exit_of(OakError::LocalCommitsNotInRemoteHistory), 5);
        assert_eq!(exit_of(OakError::RemoteCommitsNotInLocalHistory), 5);
        assert_eq!(
            exit_of(OakError::MergeInvariantViolation {
                detail: s("x"),
                next_commands: Vec::new(),
            }),
            5
        );
        assert_eq!(
            exit_of(OakError::MergePredictionUncertified {
                detail: s("x"),
                next_commands: Vec::new(),
            }),
            7
        );
        assert_eq!(exit_of(OakError::RemoteRepoNotFound(s("x"))), 6);
        assert_eq!(exit_of(OakError::RemoteRepoAlreadyExists(s("x"))), 6);
        assert_eq!(exit_of(OakError::BranchNotFound(s("x"))), 1);
        assert_eq!(exit_of(OakError::NoMergeInProgress), 1);
        assert_eq!(exit_of(OakError::MergeFailed(s("x"))), 1);
    }

    #[test]
    fn finish_preflight_and_description_stale_exits_follow_their_state() {
        use oak_core::{DescriptionStaleState as State, FinishBlocker as Blocker};
        for blocker in Blocker::ALL {
            let want = match blocker.as_str() {
                "invalid_description" => 2,
                "auth_missing" | "auth_failed" | "remote_unreachable" | "remote_repo_missing" => 6,
                _ => 1,
            };
            assert_eq!(finish_blocker_exit(*blocker).code(), want, "{blocker}");
        }
        for state in State::ALL {
            let contract = classify(&OakError::DescriptionStale {
                branch: "b".to_string(),
                state: *state,
                message: "stale".to_string(),
                refresh_commands: Vec::new(),
            });
            let unverified = *state == State::RemoteUnverified;
            assert_eq!(
                contract.exit.code(),
                if unverified { 6 } else { 5 },
                "{state}"
            );
            assert_eq!(contract.retryable, Some(unverified), "{state}");
        }
    }

    /// `retryable` is asserted only where the answer follows from what the
    /// failure is. In particular `InvalidArgument` — raised for transient
    /// server conditions too — says nothing; only a parser rejection is
    /// definitionally not retryable.
    #[test]
    fn retryable_is_only_stated_where_it_is_definitional() {
        let stated: Vec<(OakErrorKind, bool)> = sample_errors()
            .iter()
            .filter_map(|error| classify(error).retryable.map(|value| (error.kind(), value)))
            .collect();
        let mut kinds: Vec<(OakErrorKind, bool)> = stated.clone();
        kinds.sort();
        kinds.dedup();
        assert_eq!(
            kinds,
            vec![
                (OakErrorKind::WorktreeCaptureRaced, true),
                (OakErrorKind::IntegrityAdmissionBusy, true),
                (OakErrorKind::RepoLocked, true),
                (OakErrorKind::Usage, false),
                (OakErrorKind::DescriptionStale, false),
                (OakErrorKind::DescriptionStale, true),
                (OakErrorKind::ApprovalRefused, false),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        assert_eq!(
            classify(&OakError::InvalidArgument("server said retry".to_string())).retryable,
            None
        );
    }

    #[test]
    fn approval_refusals_keep_the_code_parsed_where_they_were_received() {
        let refusal = |code: ErrorCode| OakError::ApprovalRefused {
            code,
            server_code: None,
            message: "refused".to_string(),
            missing: Vec::new(),
            hidden: 0,
            recommended_next_commands: Vec::new(),
        };
        for code in ErrorCode::APPROVAL_REASONS {
            assert_eq!(classify(&refusal(*code)).code, *code);
        }
    }

    #[test]
    fn advice_depends_on_the_context_it_is_given_not_on_the_process() {
        let outside = AdviceContext {
            in_checkout: false,
            invoked: Some("oak merge --dry-run --json".to_string()),
            remote_retry: Some(RemoteRetry {
                branch: "feature".to_string(),
                command: NextCommand::BranchShowRemote {
                    name: "feature".to_string(),
                },
            }),
        };
        let http = OakError::Http("connection refused".to_string());
        assert!(classify_in(&http, &outside).next.is_empty());
        assert_eq!(classify(&http).next, vec![NextCommand::StatusJson]);
        // "Run the same command again" comes from the context, never argv.
        let busy = OakError::IntegrityAdmissionBusy("busy".to_string());
        assert_eq!(
            classify_in(&busy, &outside).rendered_next_commands(),
            vec!["oak merge --dry-run --json"]
        );
        assert!(classify(&busy).next.is_empty());
        // The `--remote` form leads only for the branch that was asked for.
        assert_eq!(
            classify_in(&OakError::BranchNotFound("feature".to_string()), &outside)
                .rendered_next_commands(),
            vec!["oak branch show feature --remote --json"]
        );
        assert!(
            classify_in(&OakError::BranchNotFound("other".to_string()), &outside)
                .next
                .is_empty()
        );
    }

    #[test]
    fn every_next_command_constructor_has_a_sample() {
        let sampled: HashSet<NextCommandKind> = NextCommand::samples()
            .iter()
            .map(NextCommand::kind)
            .collect();
        for kind in NextCommandKind::ALL {
            assert!(
                sampled.contains(kind),
                "NextCommand::samples() lacks {kind}"
            );
        }
    }

    #[test]
    fn split_command_line_inverts_shell_quote() {
        for value in ["plain", "with space", "it's", "", "a'b c", "--flag=x y"] {
            let line = format!("oak desc -- {}", shell_quote(value));
            assert_eq!(
                split_command_line(&line).unwrap(),
                vec!["oak", "desc", "--", value],
                "{line}"
            );
        }
        assert_eq!(split_command_line("oak 'unterminated"), None);
    }

    #[test]
    fn rendering_drops_duplicates_and_keeps_order() {
        let rendered = NextCommand::render_all(&[
            NextCommand::Fetch,
            NextCommand::Carried("oak fetch".to_string()),
            NextCommand::StatusJson,
        ]);
        assert_eq!(rendered, vec!["oak fetch", "oak status --json"]);
    }
}
