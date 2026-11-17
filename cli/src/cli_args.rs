//! The `oak` command line: clap's definitions of every command, subcommand
//! and argument. `main.rs` parses these and dispatches.

use super::*;

use clap::builder::styling::{AnsiColor, Styles};
use clap::Subcommand;

/// Green-on-default styling for clap's auto-generated help, so
/// `oak <command> --help` matches the hand-rolled `oak --help` banner:
/// bold-white section headers, green command/flag literals and value
/// placeholders. clap only emits these when its `color` feature is on and
/// the stream is a TTY (ColorChoice::Auto), so piped output stays clean.
pub(super) fn help_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::White.on_default().bold())
        .usage(AnsiColor::White.on_default().bold())
        .literal(AnsiColor::Green.on_default())
        .placeholder(AnsiColor::Green.on_default())
}

pub(super) const EXIT_CODE_HELP: &str = "\
Exit codes:
  0 success
  1 generic error
  2 usage error
  3 repository locked (retryable)
  4 the working tree blocked the operation: uncommitted changes, or an
    interrupted update to recover first (`oak status --json` names how)
  5 merge/sync conflicts or in-progress conflict state
  6 network, server, or auth failure
  7 merge prediction uncertified (not certified safe — no authoritative target
    head, or no prediction ran at all; run `oak fetch`)
  8 integrity proof inconclusive (the server's proof ran out of budget; not
    evidence of missing content — narrow the scope, e.g. `--shallow`/`--depth`)

Partial clones:
  oak clone <org>/<repo> --path <prefix>   sparse (partial) clone, Perforce-style
  oak sparse                               manage the checked-out cone
  OAK_ALLOW_PARTIAL_CLONE=1                 recovery: skip blobs a broken server
                                           failed to ship (instead of erroring)";

#[derive(Parser)]
#[command(name = "oak")]
#[command(about = "Oak: version control built for you and your agents")]
#[command(version = commands::update_channel::version_line())]
#[command(styles = help_styles())]
// Wrap help to the terminal, but no wider than this: long lines are hard to
// read on a wide terminal. clap propagates it to every subcommand.
#[command(max_term_width = 100)]
#[command(after_help = EXIT_CODE_HELP)]
pub(super) struct Cli {
    /// Print verbose timing info for each phase (also via `OAK_VERBOSE=1`).
    /// Note: must come before the subcommand, e.g. `oak --verbose commit`.
    #[arg(long)]
    pub(super) verbose: bool,

    #[command(subcommand)]
    pub(super) command: Commands,
}

#[derive(Subcommand)]
pub(super) enum Commands {
    /// Create a repository in the current or given directory
    Init {
        /// Directory to initialize (defaults to current directory)
        path: Option<PathBuf>,

        /// Emit one JSON receipt on stdout (never prompts): the repository
        /// path, the branch it starts on, and commits imported from git.
        #[arg(long)]
        json: bool,

        /// Import the history of the git repository in this directory
        /// without asking. Required to import when there is no terminal.
        #[arg(long, conflicts_with = "no_import_git")]
        import_git: bool,

        /// Start an empty repository beside an existing `.git` without asking.
        #[arg(long)]
        no_import_git: bool,
    },

    /// Create a local checkpoint within the current branch.
    ///
    /// Local commits no longer carry messages — branch descriptions are the
    /// source of truth for "what happened." Set the current branch's
    /// description with `oak desc "..."`.
    Commit {
        /// Commit only changes under these paths (files or directories).
        /// With no paths, every change in the working tree is committed.
        paths: Vec<PathBuf>,

        /// Refused compatibility flag: Oak commits are messageless; use `oak desc "..."` (or `oak desc --file`).
        #[arg(short = 'm', long = "message", value_name = "TEXT")]
        message: Option<String>,

        /// Skip pre-commit and post-commit hooks for this commit
        #[arg(long)]
        no_verify: bool,

        /// After checkpointing locally, explicitly publish pending branch commits
        #[arg(long)]
        push: bool,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Suppress human success/no-op text
        #[arg(long)]
        quiet: bool,
    },

    /// Squash-merge a branch into main on the server
    ///
    /// This is a remote mutation, not a local-only merge: `oak merge` first
    /// pushes BRANCH (default: the current branch), then the Oak server
    /// squash-merges it into main, advancing the remote main and closing the
    /// branch; the closed state is mirrored locally. There is no later `oak
    /// push` step. The branch description becomes the squash commit message.
    /// Nothing runs on the server before or after it; record what passed with
    /// marks (`oak mark`). Path rules that need approval can refuse the merge
    /// until an owner approves. Use `--dry-run --json` for a read-only
    /// prediction, certified against the remote main head, first.
    Merge {
        /// Continue a merge after resolving conflicts
        #[arg(long)]
        r#continue: bool,

        /// Abort a merge in progress
        #[arg(long)]
        abort: bool,

        /// Preview the merge without pushing or changing files or branches.
        /// Verifies main's head against the remote first (one request; main
        /// is fetched only when it moved) so the verdict is certified
        /// against the main this merge would land on.
        #[arg(long)]
        dry_run: bool,

        /// With --dry-run: make no network request. The prediction then
        /// uses local main as it is and is reported uncertified (exit 7).
        #[arg(long, requires = "dry_run")]
        offline: bool,

        /// Branch to merge (defaults to current branch)
        branch: Option<String>,

        /// Emit machine-readable JSON. With --dry-run this emits the merge
        /// prediction; otherwise it emits the actual server merge result.
        #[arg(long)]
        json: bool,

        /// Merge without the approvals that path rules require (the merge
        /// API's `?force=1`). Only an org admin can, and only where the
        /// repo's approval policy lets a credential do it; otherwise the
        /// refusal says where approvals are forced (often the branch page).
        #[arg(long)]
        force: bool,

        /// Accepted so older scripts keep working; it changes nothing: no
        /// check holds a merge back, so there is nothing to wait for. (It still takes an optional number of
        /// minutes: put BRANCH before it, `oak merge BRANCH --wait`.)
        #[arg(
            long,
            hide = true,
            value_name = "MINUTES",
            num_args = 0..=1,
            default_missing_value = "30",
            value_parser = parse_merge_wait_minutes
        )]
        wait: Option<u64>,
    },

    /// Switch to a branch, or detach HEAD at a commit
    ///
    /// With no name, pick a branch interactively.
    Switch {
        /// Branch name or commit hash. Omit to pick a branch interactively.
        name: Option<String>,

        /// Create a new branch and switch to it. Omit NAME to generate one.
        #[arg(short = 'c', long = "create")]
        create: bool,

        /// Start from latest available main cleanly, discarding current working-tree changes
        #[arg(long)]
        clean: bool,

        /// Deprecated alias for --clean when creating a branch.
        #[arg(long, hide = true)]
        discard: bool,

        /// Detach HEAD at the given commit (treat name as commit hash)
        #[arg(short, long)]
        detach: bool,

        /// Accepted for symmetry with `oak branch ... --remote`; behaves
        /// exactly like plain `oak switch NAME`, which already fetches NAME
        /// from the configured remote when it is not available locally.
        #[arg(long, requires = "name", conflicts_with_all = ["create", "detach"])]
        remote: bool,

        /// Emit one JSON receipt on stdout (progress on stderr): the branch
        /// and head now checked out, and the ones before. Needs NAME or -c
        /// (the interactive picker cannot run under --json).
        #[arg(long)]
        json: bool,
    },

    /// Close a branch, defaulting to the current branch
    ///
    /// With `--remote --json`, several branches can be closed checkout-free
    /// in one call by pinning each to its exact head: `oak close --remote
    /// A@FULL_HEAD B@FULL_HEAD --reason superseded --json`. Each branch is
    /// closed independently. The fence is read-then-close: a branch whose
    /// remote head differs from its pin when read is refused and nothing is
    /// sent, but the server close has no expected-head precondition, so a push
    /// landing between that read and the close is not prevented. It is
    /// detected afterwards and reported as `closed_after_head_moved` (exit 5);
    /// `fence_verified: "unchecked"` (exit 6) means the final re-check could
    /// not run. Exit 0 only when every branch is confirmed closed at its pin.
    Close {
        /// Branch name, or BRANCH@FULL_HEAD (repeatable with --remote --json)
        #[arg(value_name = "NAME")]
        names: Vec<String>,

        /// Close the branch on the configured remote without switching to it
        #[arg(long)]
        remote: bool,

        /// Explicit audit reason for closing the branch
        #[arg(long, value_name = "REASON")]
        reason: Option<String>,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Set the current branch description
    Desc {
        /// New description
        #[arg(conflicts_with = "file")]
        description: Option<String>,

        /// Read description from a UTF-8 file, or '-' for stdin
        #[arg(long, value_name = "FILE")]
        file: Option<String>,

        /// Emit a JSON receipt: branch, description, local_saved, and
        /// remote_synced (true/false/"unknown"/"not_linked") with retry guidance
        #[arg(long)]
        json: bool,

        /// Append the text (inline or --file) to the current description,
        /// after a blank line (the existing text is kept byte for byte).
        /// Refuses (`description_stale`, with a machine `state`) instead of
        /// overwriting when the local description is pending or differs from
        /// the remote one (exit 5), or when the remote copy cannot be read
        /// (`remote_unverified`, exit 6, retryable); the error names the
        /// exact refresh command.
        #[arg(long)]
        append: bool,
    },

    /// Start a task: a new branch, described and pushed
    ///
    /// Creates a branch named after SUMMARY from latest main (`oak switch -c`
    /// semantics: dirty files are carried over), sets its description to
    /// SUMMARY, and pushes it (skipped by `--no-push`). It starts from
    /// main's head.
    // Args live in their own struct so clap builds them in a separate
    // function: `Commands::augment_subcommands` is one huge stack frame in
    // debug builds, and the parse tests run on 2 MiB test threads.
    Start(StartArgs),

    /// Finalize a branch: preflight, push, description sync
    Finish {
        /// Branch description to save before finishing.
        #[arg(long, conflicts_with = "desc_file")]
        desc: Option<String>,

        /// Read branch description from a UTF-8 file, or '-' for stdin.
        #[arg(long = "desc-file", value_name = "FILE")]
        desc_file: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Manage branches
    Branch {
        /// Emit machine-readable JSON. With no subcommand, lists branches.
        #[arg(long)]
        json: bool,

        /// Print the current branch name, compatible with git branch --show-current
        #[arg(long)]
        show_current: bool,

        #[command(subcommand)]
        action: Option<BranchCommands>,
    },

    /// Agent-oriented machine-readable state
    Agent {
        #[command(subcommand)]
        action: AgentCommands,
    },

    /// Inspect and mechanically resolve in-progress conflicts
    Conflict {
        #[command(subcommand)]
        action: ConflictCommands,
    },

    /// Restore working directory files to their HEAD state
    Restore {
        /// Paths to restore (restores everything if omitted)
        paths: Vec<PathBuf>,

        /// Source commit hash or HEAD to restore from (defaults to HEAD)
        #[arg(short, long)]
        source: Option<String>,

        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,

        /// Emit one JSON receipt on stdout (the discard list goes to
        /// stderr). Cannot prompt, so it requires --force.
        #[arg(long)]
        json: bool,
    },

    /// Browse changes vs HEAD, a branch, or a commit
    ///
    /// On a terminal this opens the full-screen browser: arrow keys, vi/less
    /// keys (j/k, Space, f/b, d/u, g/G) and emacs keys (^N/^P, ^V/M-v, M-</M->)
    /// all navigate; `?` lists every binding
    // Args in their own struct: inline, its 20 flags made up a large part of
    // `Commands::augment_subcommands`'s debug stack frame (see `Start`).
    Diff(DiffArgs),

    /// Show the status of the working directory
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Emit bounded machine-readable JSON with recall metadata
        #[arg(long, requires = "json", conflicts_with_all = ["porcelain", "short"])]
        compact: bool,

        /// Emit stable compact changed-path rows for scripts
        #[arg(long, conflicts_with = "json")]
        porcelain: bool,

        /// Alias for --porcelain, compatible with git status --short
        #[arg(short = 's', long = "short", conflicts_with = "json")]
        short: bool,

        /// Apply pending remote-merge branch reconciliation before printing
        #[arg(long, conflicts_with_all = ["porcelain", "short"])]
        reconcile: bool,

        /// Add a `fingerprint` object to the --json document — the same
        /// read-only working-tree identity as `oak diff --json
        /// --fingerprint` (the id `oak change capture` would record)
        #[arg(long, requires = "json")]
        fingerprint: bool,
    },

    /// Print the current HEAD commit hash, bare, for scripts
    Hash,

    /// Show commit history
    ///
    /// On a terminal this opens the full-screen viewer: arrow keys, vi/less
    /// keys (j/k, Space, f/b, d/u, g/G) and emacs keys (^N/^P, ^V/M-v, M-</M->,
    /// ^S search, ^G cancel) all navigate; `?` lists every binding
    // Args in their own struct, like `Diff` (debug stack frame).
    Log(LogArgs),

    /// Push commits to remote server
    Push {
        /// Remote server URL. If omitted, OAK_REMOTE overrides the stored remote for this invocation.
        #[arg(short, long)]
        remote: Option<String>,

        /// Force push: overwrite remote history even when diverged
        #[arg(short, long)]
        force: bool,

        /// Link a not-yet-linked repo to ORG/REPO without the interactive
        /// org picker. ORG must be an existing organization slug; the repo
        /// is created on the server if it doesn't exist. Lets scripted /
        /// agent pushes (no TTY) link a fresh repo on first push.
        #[arg(long = "repo", value_name = "ORG/REPO", env = "OAK_REPO")]
        repo: Option<String>,

        /// Emit one machine-readable result after publication succeeds.
        #[arg(long)]
        json: bool,

        /// Read-only: report what push would send (outgoing commits with
        /// parent/merge-parent edges, trees, and which referenced blobs the
        /// server already has) using push's own planner, bound to the local
        /// head and the observed server heads. The blob fields report server
        /// presence, not the upload set: push may still send present blobs
        /// inline (small operations) or re-upload blobs lacking verified
        /// receipts. `transport` names how push would publish the plan and
        /// `publication_blocked` is set when push would refuse it. Sends no
        /// mutating request. Requires --json.
        #[arg(long, requires = "json")]
        plan: bool,
    },

    /// Update this branch, then merge in main
    ///
    /// Bring the local clone fully up to date: fetch new commits on the current
    /// branch from the remote, then merge in any changes from main.
    ///
    /// Also the recovery for a working-tree update that was killed part-way
    /// (`oak status --json` reports `progress_state.kind:
    /// "interrupted_update"`, and commit/finish/reset exit 4 with
    /// `interrupted_worktree_update` until it is recovered): `oak pull
    /// --abort` recovers and stops; `oak pull` recovers and then pulls.
    Pull {
        /// Remote server URL. If omitted, OAK_REMOTE overrides the stored remote for this invocation.
        #[arg(short, long)]
        remote: Option<String>,

        /// Force pull: discard local commits not on remote and sync with remote HEAD
        #[arg(short, long)]
        force: bool,

        /// Continue a parent-sync after resolving conflicts
        #[arg(long)]
        r#continue: bool,

        /// Abort a parent-sync in progress, or recover from an interrupted
        /// working-tree update without pulling (with --json: one receipt of
        /// what was written back, removed, and left untouched)
        #[arg(long)]
        abort: bool,

        /// Emit one JSON receipt on stdout (progress on stderr): branch,
        /// head_before/after, parent, parent_head_before/after,
        /// commits_fetched, description outcome, convergence, and unknowns.
        /// Exit codes are unchanged (a parent-sync conflict still exits 5
        /// after printing the receipt). With --continue: a small receipt
        /// (branch and head after the sync completed).
        #[arg(long)]
        json: bool,

        /// Refresh only the current branch head and metadata from the
        /// remote; skip the parent sync. Reports `parent_synced: false`, and
        /// `oak status` keeps showing that the branch still needs `oak pull`.
        #[arg(long, conflicts_with_all = ["continue", "abort"])]
        branch_only: bool,
    },

    /// Refresh the local copy of main
    ///
    /// Doesn't touch the working tree or merge anything. Useful for previewing
    /// what's new on main before deciding to `oak pull` or `oak merge`
    Fetch {
        /// Remote server URL. If omitted, OAK_REMOTE overrides the stored remote for this invocation.
        #[arg(short, long)]
        remote: Option<String>,

        /// Emit one JSON receipt on stdout (progress on stderr): local
        /// `main` head before and after, and whether it moved.
        #[arg(long)]
        json: bool,
    },

    /// Discard uncommitted changes (reset to HEAD)
    Reset {
        /// Specific path to reset (file or directory). If omitted, resets entire working directory.
        path: Option<PathBuf>,

        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,

        /// Emit one JSON receipt on stdout (the discard list goes to
        /// stderr). Cannot prompt, so it requires --force.
        #[arg(long)]
        json: bool,
    },

    /// Clone a repository from the server
    ///
    /// With no repository argument, opens an interactive repo picker.
    // Args in their own struct, like `Diff` (debug stack frame).
    Clone(CloneArgs),

    /// Check integrity: remote content, local refs, trees, files
    ///
    /// With --repo, check remote content integrity, commits to bytes.
    /// Exits 0 when verified, 6 when the proof found a problem, and 8 when
    /// the proof ran out of budget (inconclusive — narrow it with --depth).
    /// `--json` adds `outcome`: `verified`, `failed`, or `inconclusive`.
    ///
    /// The subcommands are narrower diagnostics: `refs`, `tree` and `file`
    /// check the local checkout, `blob` inspects one blob on the remote, and
    /// `compact` rewrites an old local database in the current format.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Doctor {
        #[command(subcommand)]
        action: Option<DoctorCommands>,

        /// Repository in ORG/REPO form
        #[arg(long, value_name = "ORG/REPO", required = true)]
        repo: Option<String>,

        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,

        /// Verification strength. Metadata checks graph/mapping structure;
        /// physical existence/bytes require authentication, and byte proof
        /// also requires explicit depth plus a chunk or byte budget.
        #[arg(long, value_enum, default_value = "metadata")]
        verify: commands::integrity::Verification,

        /// Limit verification to the newest N commits.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        depth: Option<u32>,

        /// Maximum chunk objects to verify in this request.
        #[arg(long, value_name = "N")]
        max_chunks: Option<usize>,

        /// Maximum object bytes to download in this request.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<u64>,

        /// Emit the structured content-integrity report
        #[arg(long)]
        json: bool,
    },

    /// List the environment variables Oak reads
    ///
    /// List the environment variables Oak itself reads plus known
    /// dependency-honored ones (proxies, HOME, TMPDIR), their effective values
    /// (secrets as presence only), and background network behaviour
    #[command(alias = "env")]
    Environment {
        #[arg(long)]
        json: bool,
    },

    /// Capture uncommitted changes without committing
    Change {
        #[command(subcommand)]
        action: ChangeCommands,
    },

    /// Manage Oak Pages sites (https://NAME.oak.page/)
    ///
    /// Oak Pages: static sites served at https://NAME.oak.page/ from a folder
    /// of a repo's main branch
    Site {
        #[command(subcommand)]
        action: SiteCommands,
    },

    /// Manage roots: separate timelines in this repo
    ///
    /// Roots: separate timelines in this repo, each with its own history. A
    /// root starts empty or from a commit on main, which is how you keep a
    /// release line that takes patches apart from main. `oak root pull NAME`
    /// puts a root's files in `roots/NAME/`, which the checkout ignores, and
    /// `oak root push` saves them back
    Root {
        #[command(subcommand)]
        action: RootCommands,
    },

    /// Manage a sparse (partial) checkout — Perforce-style.
    ///
    /// A sparse checkout scopes the working tree to a set of path prefixes
    /// (the "cone"). Files outside the cone aren't written to disk and their
    /// content isn't downloaded, but they stay in the repo and commits carry
    /// them forward untouched. With no subcommand, prints the active cone.
    Sparse {
        #[command(subcommand)]
        action: Option<SparseCommands>,

        /// Emit machine-readable JSON (for the default list view).
        #[arg(long)]
        json: bool,
    },

    /// Move and follow marks: named pointers to commits on main
    ///
    /// A mark records what has passed what (`server-good`, `cli-approved`):
    /// people and bots move it forward along main, only once the marks it
    /// requires are at or past the commit, and a rollback moves it back. A
    /// deploy is a mark a machine follows with `oak mark follow`. Marks live
    /// on the server; creating and changing them takes an organization owner
    /// or admin
    Mark {
        #[command(subcommand)]
        action: MarkCommands,

        /// The repository as OWNER/NAME, instead of this checkout's.
        #[arg(long, value_name = "OWNER/NAME", global = true)]
        repo: Option<String>,

        /// Remote server URL (with --repo).
        #[arg(short, long, global = true)]
        remote: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long, global = true)]
        json: bool,
    },

    /// Say something about a commit, or read what was said
    ///
    /// With --name and --body (or --body-file), file a report on COMMIT
    /// under that name (`tests`, `server-prod`, `review`). A report is free
    /// text, usually markdown: its first line is the headline shown on
    /// History, and the rest (a summary, numbers, the end of a log) is on the
    /// commit's page. The next report under the same name replaces it.
    /// Without a body, list the commit's reports (with --name, show that one
    /// in full). Whether a commit passed something is a mark's move, not a
    /// report.
    #[command(group(clap::ArgGroup::new("content").args(["body", "body_file"])))]
    Report {
        /// `main`, a full hash or a unique prefix.
        commit: String,
        /// What it is about (needed to file one).
        #[arg(long)]
        name: Option<String>,
        /// The report. Over 64 KiB, its start and end are kept.
        #[arg(long, requires = "name")]
        body: Option<String>,
        /// Read the report from a file (`-` for stdin).
        #[arg(long, value_name = "FILE", requires = "name")]
        body_file: Option<PathBuf>,
        /// A link to more.
        #[arg(long, requires = "content")]
        url: Option<String>,
        /// The mark move it is about.
        #[arg(long, requires = "content")]
        move_id: Option<i64>,
        /// The repository as OWNER/NAME, instead of this checkout's.
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,
        /// Remote server URL (with --repo).
        #[arg(short, long)]
        remote: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Keep only the newest versions of files in a directory
    ///
    /// Older versions keep their commits and history, but the server stops
    /// serving their content and reclaims the storage. Content an open branch
    /// head or a file outside the rule still uses is always kept. Rules
    /// live on the server; changing one needs an organization owner or admin.
    Retention {
        #[command(subcommand)]
        action: Option<RetentionCommands>,

        /// The repository as OWNER/NAME, instead of this checkout's.
        #[arg(long, value_name = "OWNER/NAME", global = true)]
        repo: Option<String>,

        /// Remote server URL (with --repo).
        #[arg(short, long, global = true)]
        remote: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long, global = true)]
        json: bool,
    },

    /// List a repo's path rules and the groups that own them
    ///
    /// List a repo's ownership boundaries: each directory or file with its
    /// owner team, whether a change there needs that team's approval
    /// (`required`, `advisory` or `off`), and who may read and write it.
    /// Boundaries live on the server; ones over paths you can't read aren't
    /// listed.
    Access {
        /// The repository as OWNER/NAME, instead of this checkout's.
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,

        /// Remote server URL (with --repo).
        #[arg(short, long)]
        remote: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show who owns a path, or what this branch touches
    ///
    /// Who owns a path: the deepest ownership boundary covering it. With
    /// --branch, the boundaries the current branch touches, with their owner
    /// teams, approval settings and approval state (the same block rides on
    /// `oak finish --json` and `oak merge --json`). Read-only.
    Owners {
        /// A repo-relative path.
        #[arg(
            value_name = "PATH",
            required_unless_present = "branch",
            conflicts_with = "branch"
        )]
        path: Option<String>,

        /// The boundaries the current branch touches.
        #[arg(long)]
        branch: bool,

        /// The repository as OWNER/NAME, instead of this checkout's.
        #[arg(long, value_name = "OWNER/NAME", conflicts_with = "branch")]
        repo: Option<String>,

        /// Remote server URL (with --repo).
        #[arg(short, long)]
        remote: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// List the groups you're in
    ///
    /// The groups you're in, across your orgs (or in --org). The one this
    /// checkout works as (`oak group switch`) is marked with `*`.
    Groups {
        /// Only this org's groups (default: all of yours).
        #[arg(long, value_name = "ORG")]
        org: Option<String>,

        /// Remote server URL (defaults to OAK_REMOTE, then oak.space).
        #[arg(long, value_name = "URL")]
        remote: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Manage an org's groups and their repo access
    ///
    /// An org's groups: who can reach which repos, and who path rules name
    /// (`@group/web`). Give a group Read or Write on a repo and everyone in it
    /// gets that. A member of a sub-group counts as a member of every group
    /// above it; a group's maintainers run it and every group below it. Every
    /// org has the built-in `public` (anyone) and `members` (everyone in the
    /// org). `oak team` is the old name.
    #[command(alias = "team")]
    Group {
        #[command(subcommand)]
        action: GroupCommands,

        /// The org (default: this checkout's).
        #[arg(long, global = true, value_name = "ORG")]
        org: Option<String>,

        /// Remote server URL (defaults to OAK_REMOTE, then oak.space).
        #[arg(long, global = true, value_name = "URL")]
        remote: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long, global = true)]
        json: bool,
    },

    /// List a directory on the remote
    ///
    /// List a directory on the remote without a clone or checkout change (e.g.
    /// a directory outside a partial checkout). Permission-filtered by the
    /// server; never changes refs or the working tree.
    Ls {
        /// Repo-relative directory (default: the repo root).
        #[arg(default_value = "")]
        path: String,
        /// Required: list the remote (there is no local `oak ls`).
        #[arg(long)]
        remote: bool,
        /// Remote branch name or full commit hash.
        #[arg(long, default_value = "main")]
        rev: String,
        /// Stop after this many entries (the JSON says `truncated`).
        #[arg(long, default_value_t = 1000)]
        max_entries: usize,
        #[arg(long)]
        json: bool,
    },

    /// Print one file from the remote
    ///
    /// Print one file from the remote, hash-verified against the pinned commit
    /// (same as `oak doctor file --remote`). Read-only.
    Cat {
        /// Repo-relative file path.
        path: String,
        /// Required: read from the remote.
        #[arg(long)]
        remote: bool,
        /// Remote branch name or full commit hash.
        #[arg(long, default_value = "main")]
        rev: String,
        /// Maximum bytes to read; a larger file is reported, not printed.
        #[arg(long, default_value_t = 16 * 1024 * 1024, value_parser = clap::value_parser!(u64).range(1..=268435456))]
        max_bytes: u64,
        #[arg(long)]
        json: bool,
    },

    /// Search the remote's main tree
    ///
    /// Search the remote's `main` tree server-side, limited to paths you can
    /// read and optionally to path prefixes after `--`. Exit 1 when nothing
    /// matches.
    Grep {
        /// Text to search for (a regex with -E).
        pattern: String,
        /// Limit results to these repo-relative path prefixes.
        #[arg(last = true)]
        paths: Vec<String>,
        /// Required: search the remote.
        #[arg(long)]
        remote: bool,
        /// Treat the pattern as a regular expression.
        #[arg(short = 'E', long)]
        regex: bool,
        /// Match case exactly (default: case-insensitive).
        #[arg(long)]
        case_sensitive: bool,
        /// Stop after this many matching files.
        #[arg(long, default_value_t = 100)]
        max_files: usize,
        #[arg(long)]
        json: bool,
    },

    /// Approve a branch's changes in paths your groups own
    ///
    /// Where a boundary's approval setting is `required`, `oak merge` is
    /// refused (HTTP 409, `approvals_required`) until someone in its owner
    /// team, who didn't work on the branch, approves the branch's changes
    /// there. Editing them needs a new approval; other pushes keep it.
    ///
    /// An approval comes from a person: use the branch page, or an API key
    /// created with the `Review` scope under Settings → API keys
    /// (`OAK_API_KEY=<key> oak approve`). `oak login` tokens, ordinary keys and
    /// org agents' keys are refused. With no --path, approves every touched
    /// boundary you may. Run from a checkout, or name the target with --repo.
    Approve {
        /// The branch (defaults to the current one).
        #[arg(value_name = "BRANCH", conflicts_with = "branch_flag")]
        branch: Option<String>,

        /// The branch, as `--repo` targets take it.
        #[arg(long = "branch", value_name = "NAME")]
        branch_flag: Option<String>,

        /// Approve one boundary, named by its path (a directory or file).
        #[arg(long = "path", value_name = "PATH")]
        boundary: Option<String>,

        /// The head you reviewed (defaults to this checkout's); a branch that
        /// has moved since is refused.
        #[arg(long, value_name = "HASH")]
        head: Option<String>,

        /// The repository as OWNER/NAME, instead of this checkout's.
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,

        /// Remote server URL (with --repo).
        #[arg(short, long)]
        remote: Option<String>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Export history into a git repository
    ///
    /// Write a branch's history into a new git repository and check it out
    /// (or, with --tree-only --at HASH, materialize one verified tree).
    ///
    /// Every commit keeps its author, message and timestamp, and merges within
    /// the branch keep both parents, so a history imported from git exports
    /// back to the same commits. Every tracked file is written, whatever
    /// .gitignore or your git hooks say.
    Export {
        /// Destination directory for the new git repo (created if missing).
        dest: PathBuf,

        /// Oak branch to export (default: current branch).
        #[arg(short, long)]
        branch: Option<String>,

        /// Name to use for the resulting git branch (default: same as the
        /// oak branch). Pass `main` if you want the export to land on
        /// git's conventional default.
        #[arg(long = "git-branch")]
        git_branch: Option<String>,

        /// Write into a destination that isn't empty. In an existing git
        /// repository this overwrites the branch and leaves its files alone.
        #[arg(short, long, conflicts_with = "tree_only")]
        force: bool,

        /// Materialize exactly one pinned tree (no history replay, no git):
        /// every file is hash-verified before the staged directory is renamed
        /// into DEST. DEST must be absent or an empty directory. Requires --at.
        #[arg(long, requires = "at", conflicts_with_all = ["branch", "git_branch"])]
        tree_only: bool,

        /// Commit to materialize with --tree-only: HEAD or a full 64-character hash
        #[arg(long, requires = "tree_only")]
        at: Option<String>,

        /// --tree-only: maximum files before refusing (nothing is written)
        #[arg(long, requires = "tree_only",
              default_value_t = oak_core::sqlite::tree_inspect::DEFAULT_TREE_MAX_FILES,
              value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
        max_files: u64,

        /// --tree-only: maximum total logical bytes before refusing
        #[arg(long, requires = "tree_only",
              default_value_t = oak_core::sqlite::tree_inspect::DEFAULT_TREE_MAX_BYTES,
              value_parser = clap::value_parser!(u64).range(0..=1_099_511_627_776))]
        max_bytes: u64,

        /// --tree-only: emit one machine-readable export receipt
        #[arg(long, requires = "tree_only")]
        json: bool,
    },

    /// Upgrade oak to the latest version
    ///
    /// Downloads from oak.space and follows your update channel: stable
    /// (tagged releases, the default) or nightly (a build of the tip of main,
    /// published once a day).
    /// `--channel` switches channel and remembers the choice for later
    /// upgrades and the daily update notice.
    Upgrade {
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
        /// Switch to this update channel (remembered), then upgrade on it
        #[arg(long, value_enum, value_name = "CHANNEL", conflicts_with = "canary")]
        channel: Option<commands::update_channel::Channel>,
        /// Install the current nightly build once, without changing your
        /// update channel (a later `oak upgrade` returns to it)
        #[arg(long)]
        canary: bool,
        /// Install the current nightly unattended (what the daily check runs
        /// in the background on the nightly channel)
        #[arg(long, hide = true, conflicts_with_all = ["force", "channel", "canary"])]
        auto: bool,
    },

    /// Log in to an Oak server
    Login {
        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Log out of an Oak server
    Logout {
        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Run an MCP server over stdio for agents
    ///
    /// Add it to a client as the command `oak mcp`, started in the repo. Tools
    /// are read-only by default. Merging and closing are never exposed.
    Mcp {
        /// Also offer the tools that commit, set the description, push, and
        /// save or try a workflow
        #[arg(long)]
        allow_write: bool,
    },

    /// Print the logged-in username for an Oak server
    Whoami {
        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,

        /// Emit machine-readable JSON (`remote`, `username`).
        #[arg(long)]
        json: bool,
    },

    /// List repositories on an Oak server (read-only)
    Repo {
        #[command(subcommand)]
        action: RepoCommands,
    },

    /// Inspect credentials and account identity (read-only)
    Auth {
        #[command(subcommand)]
        action: AuthCommands,
    },

    /// Open the repository in the web browser
    Open {
        /// Print the URL instead of launching a browser
        #[arg(long, conflicts_with = "json")]
        print: bool,

        /// Emit the URL and repository identity as JSON without launching a browser
        #[arg(long)]
        json: bool,

        /// Open the Oak Pages site NAME (https://NAME.oak.page/) instead
        #[arg(long, value_name = "NAME")]
        site: Option<String>,
    },

    /// Send feedback or a feature request to the Oak team
    ///
    /// With no -m/--file on an interactive terminal, opens $VISUAL/$EDITOR
    /// with a git-style commented template. Non-interactive callers must
    /// pass -m or --file ('-' for stdin); otherwise the command exits 2
    /// immediately instead of blocking on an editor. A contact email is
    /// optional — pass --email, set OAK_EMAIL, or answer the one-time
    /// interactive prompt (cached in ~/.oak/feedback.json).
    #[command(alias = "feature-request")]
    Feedback {
        /// Admin-only inventory, conditional transition, or link/unlink; omit to submit feedback.
        #[command(subcommand)]
        action: Option<FeedbackCommands>,

        /// Feedback text to send
        #[arg(
            short = 'm',
            long = "message",
            value_name = "TEXT",
            conflicts_with = "file"
        )]
        message: Option<String>,

        /// Read the feedback text from a UTF-8 file, or '-' for stdin
        #[arg(long, value_name = "FILE")]
        file: Option<String>,

        /// Contact email so the team can follow up (optional; defaults to
        /// OAK_EMAIL, then the cached address in ~/.oak/feedback.json)
        #[arg(long, value_name = "EMAIL")]
        email: Option<String>,

        /// Name to attribute the feedback to (defaults to your Oak identity)
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Short title for the request
        #[arg(long, value_name = "TITLE")]
        title: Option<String>,

        /// Server base URL (default: OAK_REMOTE, then the current repo's
        /// remote, then https://oak.space)
        #[arg(long, value_name = "URL")]
        remote: Option<String>,

        /// Emit a machine-readable JSON result:
        /// {"id": ..., "ref": "fb-N", "status": ...}
        #[arg(long)]
        json: bool,
    },

    /// Print a shell-completion script for `oak` to stdout.
    ///
    /// Source it from your shell's startup file to get tab-completion of
    /// subcommands and flags. For example, with bash:
    ///
    ///   oak completions bash > ~/.local/share/bash-completion/completions/oak
    ///
    /// or, to load it for the current session only:
    ///
    ///   source <(oak completions bash)
    ///
    /// zsh, fish, elvish, and powershell are supported too — pass the matching
    /// shell name.
    #[command(verbatim_doc_comment)]
    Completions {
        /// Shell to generate completions for.
        #[arg(value_name = "SHELL")]
        shell: clap_complete::Shell,
    },

    /// Split a branch's commits into independent branches
    ///
    /// Commits can also be reordered or dropped.
    ///
    /// Like `git rebase -i` / `sl histedit`, plus a split step. Opens an
    /// interactive editor by default; pass `--plan` to drive it from a
    /// todo-list file (or `-` for stdin) so an agent can run it headless.
    ///
    /// A plan is one directive per line (`#` starts a comment). Every commit
    /// on the branch must appear exactly once as `pick` or `drop`:
    ///
    ///   pick <commit>     replay this commit (prefix of the hash is fine)
    ///   drop <commit>     leave this commit out
    ///   split <branch>    start a new branch off main; later picks go there
    ///
    /// Picks before the first `split` rewrite the source branch in place.
    /// Split segments are parented on `main`, so each must stand alone on
    /// `main`; a segment that depends on another conflicts and nothing is
    /// written. Use `--dry-run` to preview the resulting structure.
    ///
    /// Example — keep two commits, carve two off into a new branch:
    ///
    ///   oak split --plan - <<'EOF'
    ///   pick a1b2c3
    ///   pick d4e5f6
    ///   split my-feature-docs
    ///   pick 778899
    ///   pick aabbcc
    ///   EOF
    #[command(alias = "histedit", verbatim_doc_comment)]
    Split {
        /// Edit this branch instead of the current one.
        #[arg(long, value_name = "BRANCH")]
        from: Option<String>,
        /// Apply a todo-list plan non-interactively (file path, or `-` for stdin).
        #[arg(long, value_name = "FILE")]
        plan: Option<String>,
        /// Print the resulting branch/commit structure without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// With --plan: emit one JSON receipt on stdout (the branches the
        /// plan wrote and their heads; `written: false` for --dry-run).
        #[arg(long, requires = "plan")]
        json: bool,
    },

    /// Manage the bundled agent skill
    ///
    /// Oak ships a `SKILL.md` (plus reference files) in the open Agent Skills
    /// format that teaches coding agents like Claude Code how to drive Oak. The
    /// files are baked into this binary, so the installed skill always
    /// documents the CLI version that wrote it
    Skill {
        #[command(subcommand)]
        action: SkillCommands,
    },
}

#[derive(Subcommand)]
pub(super) enum DoctorCommands {
    /// Verify one committed file's bytes
    ///
    /// Verify one committed file's logical bytes in a pinned local SQLite
    /// snapshot (or, with --remote, read it from one exact remote commit,
    /// hash-verified)
    File {
        /// HEAD or a full 64-character commit hash (resolved once); with
        /// --remote, a remote branch name (head pinned once) or a full hash
        #[arg(long)]
        at: String,
        /// Repository-relative file path; symlinks are not followed
        path: String,
        /// Maximum logical file bytes to verify; exhausted budgets are inconclusive
        #[arg(long, default_value_t = 16 * 1024 * 1024, value_parser = clap::value_parser!(u64).range(1..=268435456))]
        max_bytes: u64,
        #[arg(long)]
        json: bool,
        /// Read the file from the configured remote without a clone or
        /// checkout: bounded, permission-checked, verified against the
        /// pinned commit's manifest hash; prints exact bytes without --json.
        /// Local refs never change.
        #[arg(long)]
        remote: bool,
        /// Also write the verified logical bytes to this NEW file (owner-only
        /// permissions). Bytes are staged beside it and published with a
        /// no-clobber rename only after hash verification; an existing path
        /// is never overwritten and nothing is created on failure.
        #[arg(long, value_name = "FILE", conflicts_with = "remote")]
        output: Option<PathBuf>,
    },

    /// Check local refs for disagreements (read-only)
    ///
    /// Read current branch, branch heads, parent inheritance, legacy HEAD and
    /// the effective HEAD from ONE pinned local SQLite snapshot, and flag any
    /// disagreement between them. Read-only; no network.
    Refs {
        /// List at most this many branches (sorted by name)
        #[arg(long, default_value_t = oak_core::sqlite::refs_inspect::DEFAULT_REFS_MAX_BRANCHES,
              value_parser = clap::value_parser!(u64).range(1..=100_000))]
        max_branches: u64,
        #[arg(long)]
        json: bool,
    },

    /// List a commit's paths, sizes, and hashes
    ///
    /// Verify and list one pinned commit's complete path set: modes, logical
    /// sizes, BLAKE3 blob ids and SHA-256 of verified bytes. Bounded; a budget
    /// stop is reported as explicit truncation. Local only.
    Tree {
        /// HEAD or a full 64-character commit hash (resolved once)
        #[arg(long)]
        at: String,
        /// Maximum files to list before truncating
        #[arg(long, default_value_t = oak_core::sqlite::tree_inspect::DEFAULT_TREE_MAX_FILES,
              value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
        max_files: u64,
        /// Maximum total logical bytes to verify before truncating
        #[arg(long, default_value_t = oak_core::sqlite::tree_inspect::DEFAULT_TREE_MAX_BYTES,
              value_parser = clap::value_parser!(u64).range(0..=1_099_511_627_776))]
        max_bytes: u64,
        #[arg(long)]
        json: bool,
    },

    /// Inspect one blob's metadata, chunks, and paths
    ///
    /// Prove one blob's metadata, mapping, chunks, objects, and affected paths
    Blob {
        /// Blob content hash
        hash: String,

        /// Repository in ORG/REPO form
        #[arg(long, value_name = "ORG/REPO")]
        repo: String,

        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,

        /// Inspect at most this many newest commits on the selected branch
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10_000))]
        depth: Option<u32>,

        /// Select a branch for a bounded --depth inspection
        #[arg(long, requires = "depth")]
        branch: Option<String>,

        /// Emit the structured content-integrity report
        #[arg(long)]
        json: bool,
    },

    /// Compact the local database and reclaim space
    ///
    /// Compact the local database: convert to the compact tree/blob storage
    /// format, drop the legacy tree_entries table, and VACUUM to reclaim space.
    Compact,
}

#[derive(Subcommand)]
pub(super) enum ChangeCommands {
    /// Store working-tree changes without committing
    ///
    /// Capture working-tree changes as durable blobs and a tree without
    /// changing refs
    Capture {
        /// Limit the captured result to these repository-relative files or directories
        paths: Vec<PathBuf>,
        /// Emit the versioned capture receipt as JSON
        #[arg(long)]
        json: bool,
    },
    /// Export one capture as a self-contained archive
    Export {
        /// Durable local capture occurrence identifier
        capture_id: String,
        /// New archive file to create without replacing an existing path
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
        /// Emit the versioned export receipt as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum BranchCommands {
    /// List branches
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Read branch metadata from the configured remote
        #[arg(long)]
        remote: bool,

        /// Filter by branch status, e.g. open or closed
        #[arg(long)]
        status: Option<String>,
    },

    /// Show one branch
    Show {
        /// Branch name
        name: String,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Read branch metadata from the configured remote
        #[arg(long)]
        remote: bool,
    },

    /// Show a checkout-free diff summary for a branch
    Diff {
        /// Branch name
        name: String,

        /// Branch to compare against
        #[arg(long, default_value = "main")]
        against: String,

        /// Diff perspective: tree (target head vs branch), contribution (fork vs branch), or net-merge (target vs predicted merge)
        #[arg(long = "diff-mode", default_value = "tree")]
        diff_mode: String,

        /// Limit the number of changed file summaries in JSON output
        #[arg(long = "changed-files-limit")]
        changed_files_limit: Option<usize>,

        /// Start changed-file JSON output at this zero-based offset
        #[arg(long = "changed-files-offset", default_value_t = 0)]
        changed_files_offset: usize,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Include unified hunks per file in --json output
        #[arg(long, requires = "json")]
        hunks: bool,

        /// Byte budget across all --hunks (or --remote --print) patches
        #[arg(long)]
        max_bytes: Option<usize>,

        /// Number of context lines around hunks
        #[arg(short = 'U', long = "unified", value_name = "N")]
        unified: Option<usize>,

        /// Treat oversized text files as text in --hunks (or --remote
        /// --print) output. NUL-containing binary files still stay binary.
        #[arg(short = 'a', long = "text")]
        text: bool,

        /// Review the configured remote branch without switching to it.
        /// Pins both heads from one branch listing; with --hunks/--print it
        /// hydrates only the returned files' content (bounded, hash-verified)
        /// into the object cache. Local refs are never changed.
        #[arg(long)]
        remote: bool,

        /// With --remote: print the pinned diff as a unified patch instead of
        /// JSON. Files whose patch is omitted are named on stderr with why.
        #[arg(long, requires = "remote", conflicts_with = "json")]
        print: bool,

        /// Number hunk lines: structured `hunks` per file with
        /// --hunks, or old/new line-number gutters with --remote --print
        #[arg(long)]
        line_numbers: bool,

        /// With --remote: the repository as OWNER/NAME, so no checkout is
        /// needed (default: this checkout's repository)
        #[arg(long, requires = "remote", value_name = "OWNER/NAME")]
        repo: Option<String>,

        /// Limit the diff to these files or directories
        paths: Vec<PathBuf>,
    },

    /// Show checkout-free branch review evidence
    Review {
        /// Branch name
        name: String,

        /// Include local merge conflict prediction
        #[arg(long)]
        merge_preview: bool,

        /// Limit the number of changed file summaries in JSON output
        #[arg(long = "changed-files-limit")]
        changed_files_limit: Option<usize>,

        /// Start changed-file JSON output at this zero-based offset
        #[arg(long = "changed-files-offset", default_value_t = 0)]
        changed_files_offset: usize,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Review the remote branch without switching to it. The server
        /// computes the file list and merge verdict; no checkout or object
        /// download is needed (older servers fall back to local evidence).
        #[arg(long)]
        remote: bool,

        /// With --remote: the repository as OWNER/NAME, so no checkout is
        /// needed (default: this checkout's repository)
        #[arg(long, requires = "remote", value_name = "OWNER/NAME")]
        repo: Option<String>,
    },

    /// Triage many branches without switching
    Triage {
        /// Read branch metadata from the configured remote
        #[arg(long)]
        remote: bool,

        /// Branch to compare against
        #[arg(long, default_value = "main")]
        against: String,

        /// Filter by branch status, e.g. open or closed
        #[arg(long)]
        status: Option<String>,

        /// How deeply to analyze each branch
        #[arg(long = "analysis-depth", default_value = "manifest")]
        analysis_depth: String,

        /// Return only rows matching a triage bucket
        #[arg(long)]
        only: Option<String>,

        /// Maximum number of branches to analyze in depth
        #[arg(long)]
        limit: Option<usize>,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Rename a branch
    Rename {
        /// Existing branch name
        old_name: String,

        /// New branch name
        new_name: String,
    },
}

#[derive(Subcommand)]
pub(super) enum AgentCommands {
    /// Print the repo's state and next actions as JSON
    ///
    /// Print one compact JSON document with the next useful agent actions.
    State {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Refresh remote freshness fields before printing JSON.
        #[arg(long)]
        refresh: bool,

        /// Omit null/default fields and redundant aliases from JSON output.
        #[arg(long)]
        compact: bool,

        /// Continue the bounded publication-attempt page after this operation.
        #[arg(long, value_name = "OPERATION_ID")]
        publication_after: Option<String>,
    },

    /// Manage local records of interrupted publishes
    ///
    /// Manage local interrupted-publication evidence without remote mutation.
    Publication {
        #[command(subcommand)]
        action: AgentPublicationCommands,
    },
}

#[derive(Subcommand)]
pub(super) enum AgentPublicationCommands {
    /// Forget a local publish record whose outcome is unknown
    ///
    /// Forget one exact local record after accepting its outcome remains
    /// unknown.
    Forget {
        operation_id: String,
        #[arg(long)]
        acknowledge_unknown: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum FeedbackCommands {
    /// Change a feedback ticket's status (admin)
    ///
    /// Conditionally edit one ticket using an explicitly observed revision
    /// (admin-only).
    ///
    /// Requires feedback_transition_v1 on the server. Conflicts and UNCONFIRMED
    /// outcomes require refresh and reconciliation; no automatic retry or fallback.
    Transition {
        /// Ticket reference: fb-N, N, or storage ID.
        item: String,
        /// Exact opaque token from feedback show/list; never replaced by a later read.
        #[arg(long, value_name = "TOKEN")]
        expected_revision: String,
        /// New status: new, triaged, reviewed, planned, in_progress, blocked, done, wontfix, duplicate, spam.
        #[arg(long)]
        status: Option<String>,
        /// Replace notes (20,000 characters max); omit to preserve. Empty text clears.
        #[arg(long, conflicts_with = "clear_notes")]
        notes: Option<String>,
        /// Explicitly clear notes without changing omitted fields.
        #[arg(long)]
        clear_notes: bool,
        /// Server URL; credentials are scoped to this remote and redirects are refused.
        #[arg(long, value_name = "URL")]
        remote: Option<String>,
        /// Emit the validated minimal receipt, excluding private note content.
        #[arg(long)]
        json: bool,
    },
    /// List the feedback queue (admin)
    ///
    /// Read a bounded platform-admin work queue (reporter metadata omitted).
    List {
        #[arg(long, default_value = "nonterminal")]
        status: String,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
        /// Opaque next-page cursor returned by the preceding list response.
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, value_name = "URL")]
        remote: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one feedback report (admin)
    ///
    /// Read one feedback report, using fb-N, N, or a storage ID (admin-only).
    Show {
        item: String,
        #[arg(long, value_name = "URL")]
        remote: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Link a feedback item to a branch or commit
    ///
    /// Admin-only: the server answers everyone else with a quiet 404, which
    /// this reports as "not authorized (or unknown item)".
    Link {
        /// The feedback item: fb-165, 165, or its raw id.
        #[arg(value_name = "ITEM")]
        item: String,

        /// Branch that addresses the item (default: the current branch).
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,

        /// Repository as <org>/<repo> (default: the current checkout's repo)
        #[arg(long, value_name = "ORG/REPO")]
        repo: Option<String>,

        /// Commit hash to record. On its own (no --branch) this files a
        /// commit link instead of a branch link.
        #[arg(long, value_name = "HASH")]
        commit: Option<String>,

        /// Server base URL (default: OAK_REMOTE, then the current repo's
        /// remote, then https://oak.space)
        #[arg(long, value_name = "URL")]
        remote: Option<String>,

        /// Emit a machine-readable JSON result
        #[arg(long)]
        json: bool,
    },

    /// Unlink a feedback item from a branch or commit
    ///
    /// Name the link one of three ways: --link-id (exact, as printed by
    /// `link --json`), --commit (matches commit-only *and* branch+commit
    /// links), or --branch (default: the current branch). An ambiguous
    /// match is never guessed at — it is reported with each candidate's link
    /// id and the exact --link-id command that resolves it.
    Unlink {
        /// The feedback item: fb-165, 165, or its raw id.
        #[arg(value_name = "ITEM")]
        item: String,

        /// The link's own id, as printed by `link --json` and by an
        /// ambiguous unlink. Identifies the link outright.
        #[arg(
            long = "link-id",
            value_name = "ID",
            conflicts_with_all = ["branch", "commit"]
        )]
        link_id: Option<String>,

        /// Commit hash to unlink. Matches commit-only links and
        /// branch-plus-commit links alike.
        #[arg(long, value_name = "HASH", conflicts_with = "branch")]
        commit: Option<String>,

        /// Branch to unlink (default: the current branch).
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,

        /// Repository as <org>/<repo> (default: the current checkout's
        /// repo). Narrows --branch/--commit matching; ignored by --link-id.
        #[arg(long, value_name = "ORG/REPO")]
        repo: Option<String>,

        /// Server base URL (default: OAK_REMOTE, then the current repo's
        /// remote, then https://oak.space)
        #[arg(long, value_name = "URL")]
        remote: Option<String>,

        /// Emit a machine-readable JSON result
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum ConflictCommands {
    /// Summarize any in-progress conflict
    ///
    /// Summarize any in-progress merge or pull sync conflict.
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Show per-path facts for any in-progress conflict.
    Show {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Resolve a conflicted file by taking one side
    ///
    /// Resolve one checkout conflict marker file by taking one side.
    Take {
        /// Repo-relative, cwd-relative, or absolute path to a conflicted file.
        path: PathBuf,

        /// Keep this branch's side of every marker block.
        #[arg(long, conflicts_with = "theirs")]
        ours: bool,

        /// Keep the parent/remote side of every marker block.
        #[arg(long)]
        theirs: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum RepoCommands {
    /// List repositories visible to you
    ///
    /// List the repositories visible to you on the remote. Without a login only
    /// public repositories are listed.
    List {
        /// Only list repositories owned by this org
        #[arg(long, value_name = "ORG")]
        org: Option<String>,

        /// Maximum rows to emit (the payload reports `truncated`)
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..=5000))]
        limit: u32,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Show or change whether a repo is public
    ///
    /// With no argument, prints the current visibility. `public` or
    /// `private` changes it; only owners and admins of the repo's
    /// organization may. Defaults to this checkout's repo.
    Visibility {
        /// New visibility; omit to show the current one
        #[arg(value_enum)]
        visibility: Option<commands::repo_visibility::Visibility>,

        /// Repository as OWNER/NAME (default: this checkout's repo)
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Move a repo, with history, into another repo
    ///
    /// Move a repo, with its history, into a directory of another repo. The
    /// source is archived and records where it went; the directory gets an path
    /// rule owned by --owner-group. Org owners and admins only; with the
    /// default force policy the start has to come from the web.
    Move {
        /// The repo to move, as ORG/REPO.
        #[arg(value_name = "ORG/REPO")]
        source: String,

        /// Where it goes: ORG/REPO:PATH (e.g. `acme/mono:apps/web`).
        #[arg(long, value_name = "ORG/REPO:PATH")]
        into: String,

        /// The group that owns the directory afterwards (`--owner-team`
        /// still works).
        #[arg(long = "owner-group", alias = "owner-team", value_name = "GROUP")]
        owner_team: String,

        /// Show what it would do. Changes nothing.
        #[arg(long)]
        dry_run: bool,

        /// Start it and return without following it.
        #[arg(long)]
        no_wait: bool,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Follow a moved repo with a new checkout
    ///
    /// Carry this checkout's work to where its repo moved: a new checkout of
    /// that directory beside this one, with this branch's commits as one commit
    /// and dirty files left dirty. This checkout isn't touched.
    Adopt {
        /// Where to put the new checkout (default: beside this one).
        #[arg(long, value_name = "DIR")]
        into: Option<std::path::PathBuf>,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum AuthCommands {
    /// Show which credential is in use and who you are
    ///
    /// Report the credential source (never the secret), the remote, the
    /// server-resolved identity, and whether feedback-admin routes answer.
    /// Exits 0 whenever a report was produced, even when unauthenticated.
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Remote server URL (default: OAK_REMOTE, then this checkout's
        /// remote, then https://oak.space)
        #[arg(short, long)]
        remote: Option<String>,
    },
}

#[derive(clap::Args)]
pub(super) struct DiffArgs {
    /// Revisions and/or path filters. Up to two leading arguments that
    /// name a branch or commit (unique hash prefix, ≥ 4 hex chars)
    /// select the diff endpoints — `oak diff <branch>` shows the
    /// branch's contribution checkout-free, `oak diff <commit>` compares
    /// it to the working tree, `oak diff <rev> <rev>` diffs two
    /// revisions. Remaining arguments (or everything after `--`) limit
    /// the diff to those files or directories
    pub(super) paths: Vec<PathBuf>,

    /// Base to compare a branch endpoint against (defaults to the
    /// branch's parent)
    #[arg(long)]
    pub(super) against: Option<String>,

    /// Diff perspective for branch endpoints: contribution (fork point
    /// vs branch — what the branch did; default for `oak diff
    /// <branch>`), tree (base head vs branch head; default for two
    /// revisions), or net-merge (base vs predicted merge result)
    #[arg(long)]
    pub(super) mode: Option<String>,

    /// Emit machine-readable JSON summary
    #[arg(long)]
    pub(super) json: bool,

    /// Return at most N changed-file summaries in --json output
    #[arg(long, requires = "json")]
    pub(super) changed_files_limit: Option<usize>,

    /// Start --json changed-file summaries at this offset
    #[arg(long, requires = "json", default_value_t = 0)]
    pub(super) changed_files_offset: usize,

    /// Include unified hunks per file in --json output (progressive
    /// disclosure: summary first, then hunks — scope with path filters
    /// to fetch one file's patch)
    #[arg(long, requires = "json")]
    pub(super) hunks: bool,

    /// Byte budget across all --hunks patches; files past the budget
    /// carry "patch_omitted": true and can be fetched individually
    #[arg(long, requires = "hunks")]
    pub(super) max_bytes: Option<usize>,

    /// Print the diff to stdout instead of opening the interactive
    /// browser. Combines with --name-only / --stat, which are already
    /// non-interactive and take precedence over full hunks
    #[arg(long)]
    pub(super) print: bool,

    /// Show per-file +added/-removed line counts instead of full hunks
    #[arg(long, conflicts_with = "name_only")]
    pub(super) stat: bool,

    /// Show only changed file paths
    #[arg(long, conflicts_with = "json")]
    pub(super) name_only: bool,

    /// Diff the whole branch — its commits plus uncommitted changes —
    /// against its fork point on the parent branch, instead of only
    /// uncommitted changes vs HEAD
    #[arg(long)]
    pub(super) branch: bool,

    /// Exit with code 1 when differences exist and 0 when there are
    /// none (like `git diff --exit-code`), for scripting without
    /// parsing output
    #[arg(long)]
    pub(super) exit_code: bool,

    /// Number of context lines around hunks in printed/browsed diffs
    #[arg(short = 'U', long = "unified", value_name = "N")]
    pub(super) unified: Option<usize>,

    /// Mark intra-line changes in printed hunks with [-old-] / {+new+}
    #[arg(long, requires = "print")]
    pub(super) word_diff: bool,

    /// Treat oversized text files as text instead of showing the
    /// large-file notice. NUL-containing binary files still stay binary.
    #[arg(short = 'a', long = "text")]
    pub(super) text: bool,

    /// Check added lines for whitespace errors (trailing whitespace,
    /// space before tab in indent, new blank lines at EOF) and
    /// unresolved conflict markers instead of showing the diff. Prints
    /// `path:line: problem`, or with --json one document listing
    /// {path, line, kind}. Exits 1 when problems are found, 0 when clean
    #[arg(
        long,
        conflicts_with_all = ["name_only", "stat", "hunks", "word_diff", "fingerprint", "verify_fingerprint", "exit_code"]
    )]
    pub(super) check: bool,

    /// Add a `fingerprint` object to the working-tree --json document:
    /// {change_id, base_commit, base_tree, result_tree,
    /// changed_file_count}. `change_id` is the id `oak change capture`
    /// would record for the same tree and paths; computing it writes
    /// nothing and needs no linked remote
    #[arg(long, requires = "json", conflicts_with = "branch")]
    pub(super) fingerprint: bool,

    /// Recompute the working-tree fingerprint (optionally scoped to
    /// paths) and exit 1 unless its change_id equals ID. Writes nothing
    #[arg(
        long,
        value_name = "ID",
        conflicts_with_all = ["branch", "name_only", "stat", "print", "hunks", "fingerprint"]
    )]
    pub(super) verify_fingerprint: Option<String>,

    /// Number hunk lines. With --json --hunks, each file with a
    /// patch also gets structured `hunks`:
    /// [{old_start, old_len, new_start, new_len, lines: [{kind,
    /// old_line, new_line, text}]}], bounded by the same --max-bytes
    /// budget. With --print, hunk lines are prefixed with their old/new
    /// line numbers (the output is then for reading, not `apply`)
    #[arg(long, conflicts_with_all = ["check", "stat", "name_only"])]
    pub(super) line_numbers: bool,

    /// Diff two exact commits read from the configured remote without
    /// checking them out: `oak diff --remote OLD NEW` with full
    /// commit hashes. Fetches only those two commits' manifests and, with
    /// --hunks/--print, the returned files' content (bounded,
    /// hash-verified) into the object cache. Local refs never change.
    /// Requires --json or --print
    #[arg(
        long,
        conflicts_with_all = ["branch", "against", "mode", "check", "fingerprint", "verify_fingerprint", "stat", "name_only", "word_diff"]
    )]
    pub(super) remote: bool,
}

#[derive(clap::Args)]
pub(super) struct CloneArgs {
    /// Remote server URL
    #[arg(short, long)]
    pub(super) remote: Option<String>,

    /// Repository spec in `org/repo` form (e.g. `oak/oak`). A bare
    /// `repo` is allowed when logged in — it defaults to your personal
    /// organization (your username as org). Omit to search and pick.
    #[arg(value_name = "ORG/REPO")]
    pub(super) name: Option<String>,

    /// Destination directory. Defaults to the repo name in the current
    /// directory (git-style — `oak clone oak/foo` clones into `./foo`).
    pub(super) dest: Option<PathBuf>,

    /// After cloning, switch to this remote branch.
    #[arg(long, value_name = "NAME")]
    pub(super) branch: Option<String>,

    /// Require the selected branch to be open at this exact full Oak
    /// commit hash. The server verifies the pin before any clone files
    /// are created and revalidates it when serving the snapshot. Requires
    /// --branch NAME, or --detached to pin the default branch (main).
    #[arg(long, value_name = "FULL")]
    pub(super) expected_head: Option<String>,

    /// Clone only the newest commit on the selected branch (the default
    /// branch when --branch is omitted), like `git clone --depth=1`.
    /// This narrows local history plus preflight/recovery scope; the
    /// selected commit's working tree remains complete. By default `oak
    /// clone` downloads the full reachable history.
    #[arg(long)]
    pub(super) shallow: bool,

    /// Proceed when the server's bounded metadata preflight is inconclusive
    /// because it exhausted only its history/tree/path or wall-time
    /// budget. Completeness is then not proved; the download stays pinned
    /// to the proof's snapshot and pull still hash-verifies objects.
    /// Corruption findings are never overridden. Without this flag such a
    /// clone exits 8 and recommends the exact `--shallow` retry.
    #[arg(long)]
    pub(super) allow_unverified_integrity: bool,

    /// Permit a scoped clone when an accessible legacy server lacks the
    /// bounded proof capability. Pull may honor the requested scope and
    /// still verifies hashes; this waives only proof before download.
    #[arg(long)]
    pub(super) allow_legacy_scope: bool,

    /// Sparse (partial) clone: check out only the files under these path
    /// prefixes, Perforce-style. Repeatable (`--path src --path docs/api`)
    /// or comma-separated. The repo's full tree is still listed, but the
    /// content of out-of-cone files is not downloaded or written to disk —
    /// commits carry those paths forward untouched. Manage the cone later
    /// with `oak sparse`.
    #[arg(long = "path", value_name = "PREFIX")]
    pub(super) paths: Vec<String>,

    /// Deprecated no-op: full history is now the default. Accepted so
    /// existing `oak clone --full` invocations keep working.
    #[arg(long, hide = true, conflicts_with = "shallow")]
    pub(super) full: bool,

    /// Emit one bounded machine-readable acquisition receipt on success
    #[arg(long)]
    pub(super) json: bool,

    /// Read-only acquisition: land on a detached HEAD at the default
    /// branch's head instead of creating a personal branch. `oak commit`
    /// refuses until you run `oak switch -c`. With --expected-head FULL the
    /// default branch must be open at that exact head, and the clone lands
    /// there.
    #[arg(long, conflicts_with = "branch")]
    pub(super) detached: bool,

    /// Seed content from an existing local checkout of the same repository
    /// (same owner/name and remote origin) instead of downloading it. Only
    /// content chunks are reused, each re-hashed against the server's
    /// descriptors; commits, trees, refs and the head still come from the
    /// server, and no credentials, locks, refs or dirty files are copied.
    /// The receipt's `seed` block reports reuse counts.
    #[arg(long = "from", value_name = "LOCAL_CHECKOUT")]
    pub(super) from: Option<PathBuf>,
}

#[derive(clap::Args)]
pub(super) struct LogArgs {
    /// Maximum number of commits to show
    #[arg(short = 'n', long)]
    pub(super) limit: Option<usize>,

    /// Show verbose output including changed files
    #[arg(short, long, conflicts_with = "oneline")]
    pub(super) verbose: bool,

    /// Show one compact commit per line
    #[arg(long, conflicts_with = "json")]
    pub(super) oneline: bool,

    /// Emit machine-readable JSON
    #[arg(long)]
    pub(super) json: bool,

    /// Only show commits touching these paths (files or directories)
    #[arg(value_name = "PATH")]
    pub(super) paths: Vec<String>,

    /// Only show commits whose changes add or remove occurrences of TERM
    /// (like `git log -S`). -S counts literal occurrences; use -G to match
    /// a regex against changed lines instead
    #[arg(short = 'S', long = "search", value_name = "TERM")]
    pub(super) search: Option<String>,

    /// Only show commits whose diff has an added or removed line matching
    /// PATTERN (like `git log -G`). -G matches a regex against changed
    /// lines; -S counts literal occurrences
    #[arg(
        short = 'G',
        long = "search-regex",
        value_name = "PATTERN",
        conflicts_with = "search"
    )]
    pub(super) search_regex: Option<String>,

    /// Read the history of a remote branch without fetching it: a
    /// bounded, read-only first-parent walk from a head pinned by one
    /// branch listing. Each row is hash-verified and carries
    /// merge_source_branch (the merge parent's branch) and the fb-N
    /// feedback_refs parsed from its message; truncation is explicit, with
    /// a --from continuation. Requires --json; -n defaults to 20 (max 200)
    #[arg(
        long,
        requires = "json",
        conflicts_with_all = ["verbose", "oneline", "paths", "search", "search_regex"]
    )]
    pub(super) remote: bool,

    /// With --remote: the branch to walk (default main)
    #[arg(long, requires = "remote")]
    pub(super) branch: Option<String>,

    /// With --remote: start from this exact full commit hash instead of
    /// the listed branch head (continuation from next_command)
    #[arg(long, requires = "remote")]
    pub(super) from: Option<String>,
}

#[derive(clap::Args)]
pub(super) struct StartArgs {
    /// One-line task summary; becomes the branch name (slugified) and
    /// description.
    pub(super) summary: String,

    /// Branch name to use instead of the slugified summary.
    #[arg(long)]
    pub(super) name: Option<String>,

    /// Do not push the new branch.
    #[arg(long)]
    pub(super) no_push: bool,

    /// Start from latest main cleanly, discarding working-tree changes.
    #[arg(long)]
    pub(super) clean: bool,

    /// Accepted for older scripts and skill copies: a task always starts
    /// from main's head, so the workflows named here are ignored.
    #[arg(
        long,
        hide = true,
        value_name = "WORKFLOWS",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub(super) good: Option<String>,

    /// Accepted for older scripts: a task always starts from main's head.
    #[arg(long, hide = true)]
    pub(super) tip: bool,

    /// Emit a machine-readable receipt.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Subcommand)]
pub(super) enum SkillCommands {
    /// Install the agent skill in this repo or globally
    ///
    /// Install the `oak-vcs` agent skill into the current repo's
    /// `.claude/skills/` (project-level — commit it so every collaborator's
    /// agent picks it up), or with `--global` into `~/.claude/skills/` for
    /// every project on this machine. Re-running refreshes stale files after an
    /// `oak upgrade`; up-to-date files are left untouched.
    Install {
        /// Install user-wide (~/.claude/skills) instead of into the current repo.
        #[arg(long)]
        global: bool,
        /// Print one receipt: path, files written, and the next command.
        #[arg(long)]
        json: bool,
    },
    /// Show whether installed copies match this CLI
    ///
    /// Reports the copy in this repo's `.claude/skills/` (when run inside a
    /// repo) and the user-wide one in `~/.claude/skills/`, each as `current`,
    /// `stale` or `missing`. A copy goes stale when the CLI is upgraded; the
    /// output names the install command that refreshes it.
    Status {
        /// Print one JSON document with every copy and its state.
        #[arg(long)]
        json: bool,
    },
    /// Print a bundled skill file to stdout
    ///
    /// Prints SKILL.md by default, or a reference file such as
    /// `references/commands.md` (or just `commands.md`). Useful for agents
    /// that don't load skills from disk: they read the copy that matches this
    /// binary without installing anything.
    Show {
        /// The file to print (default: SKILL.md).
        file: Option<String>,
    },
}

/// Flags shared by every `oak root` subcommand.
#[derive(clap::Args, Clone, Default)]
pub(super) struct RootTargetArgs {
    /// Repository as OWNER/NAME (default: this checkout's repo, or the one
    /// the root directory you're in belongs to)
    #[arg(long, value_name = "OWNER/NAME")]
    pub(super) repo: Option<String>,

    /// Remote server URL
    #[arg(short, long)]
    pub(super) remote: Option<String>,

    /// Machine-readable output (list/show/create/archive print the server's
    /// JSON unchanged)
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Subcommand)]
pub(super) enum RootCommands {
    /// List the roots you can see
    #[command(alias = "ls")]
    List {
        /// Include archived roots
        #[arg(long)]
        archived: bool,

        #[command(flatten)]
        target: RootTargetArgs,
    },

    /// Show a root's owner, head, and your access
    ///
    /// Show one root: its owner group, write mode, where it started, its head
    /// and your access
    Show {
        /// Root name (`~name` or `name`)
        root: String,

        #[command(flatten)]
        target: RootTargetArgs,
    },

    /// Create a root, empty or from a commit on main
    ///
    /// Create a root. It starts empty, or with --from at a commit on main
    /// (`main` for main's current head), which is how you start a release line
    Create {
        /// Root name (`~name` or `name`)
        name: String,

        /// The group that writes it. Without one, only the org's owners and
        /// admins can push to it
        #[arg(long = "owner-group", value_name = "GROUP")]
        owner_group: Option<String>,

        /// Start from this commit on main's first-parent history: `main`, or
        /// a full 64-character commit hash
        #[arg(long, value_name = "main|HASH")]
        from: Option<String>,

        /// Make it a sync root instead of a direct one
        #[arg(long)]
        sync: bool,

        #[command(flatten)]
        target: RootTargetArgs,
    },

    /// Archive a root so it takes no more pushes
    Archive {
        /// Root name (`~name` or `name`)
        root: String,

        /// Confirm archiving
        #[arg(long, short = 'y')]
        yes: bool,

        #[command(flatten)]
        target: RootTargetArgs,
    },

    /// Bring a root's files into a local directory
    ///
    /// Bring a root's files into a local directory (default `roots/NAME/` at
    /// the checkout root). Files you changed locally are kept; a file changed
    /// on both sides refuses the pull (exit 5) unless --force
    Pull {
        /// Root name (default: the root of the directory you're in)
        root: Option<String>,

        /// Directory to hold the root
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,

        /// On a file changed both here and on the root, take the root's copy
        #[arg(long)]
        force: bool,

        #[command(flatten)]
        target: RootTargetArgs,
    },

    /// Save a root's directory as a commit on the root
    ///
    /// Save a root directory as one commit on the root (only new file content
    /// is sent). If the root moved since your last pull, exits 5: pull, then
    /// push again
    Push {
        /// Root name (default: the root of the directory you're in)
        root: Option<String>,

        /// Directory holding the root
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,

        #[command(flatten)]
        target: RootTargetArgs,
    },
}

impl RootCommands {
    /// The shared flags, the action, and whether a destructive action was
    /// confirmed.
    pub(super) fn into_action(self) -> (RootTargetArgs, commands::root::RootAction, bool) {
        use commands::root::RootAction;
        match self {
            RootCommands::List { archived, target } => {
                (target, RootAction::List { archived }, true)
            }
            RootCommands::Show { root, target } => (target, RootAction::Show { root }, true),
            RootCommands::Create {
                name,
                owner_group,
                from,
                sync,
                target,
            } => (
                target,
                RootAction::Create {
                    name,
                    owner_group,
                    from,
                    sync,
                },
                true,
            ),
            RootCommands::Archive { root, yes, target } => {
                (target, RootAction::Archive { root }, yes)
            }
            RootCommands::Pull {
                root,
                dir,
                force,
                target,
            } => (target, RootAction::Pull { root, dir, force }, true),
            RootCommands::Push { root, dir, target } => {
                (target, RootAction::Push { root, dir }, true)
            }
        }
    }
}

/// Flags shared by every `oak site` subcommand.
#[derive(clap::Args, Clone, Default)]
pub(super) struct SiteTargetArgs {
    /// Repository as OWNER/NAME (default: this checkout's repo)
    #[arg(long, value_name = "OWNER/NAME")]
    pub(super) repo: Option<String>,

    /// Remote server URL
    #[arg(short, long)]
    pub(super) remote: Option<String>,

    /// Print the server's JSON response
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Subcommand)]
pub(super) enum SiteCommands {
    /// List this repo's sites (or an organization's, with --org)
    List {
        /// List every site of this organization instead of this repo's
        #[arg(long, value_name = "ORG")]
        org: Option<String>,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Create a site from a folder of main
    ///
    /// Create a site serving a folder of this repo's main branch at
    /// https://NAME.oak.page/
    Create {
        /// Globally unique site name (lowercase letters, digits, '-')
        name: String,

        /// Repo-relative folder to serve (default: /)
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,

        /// Serve /index.html for paths that match no file (single-page apps)
        #[arg(long)]
        spa: bool,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Show a site's settings and live deployment
    Show {
        name: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Rename a site (its URL changes to https://NEW.oak.page/)
    Rename {
        name: String,

        /// The new site name
        new_name: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Change a site's folder, SPA fallback, or password
    Set {
        name: String,

        /// Repo-relative folder to serve
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,

        /// Turn the single-page-app fallback on
        #[arg(long, conflicts_with = "no_spa")]
        spa: bool,

        /// Turn the single-page-app fallback off
        #[arg(long)]
        no_spa: bool,

        /// Require HTTP basic auth with this username (needs --password-file)
        #[arg(
            long,
            value_name = "USER",
            requires = "password_file",
            conflicts_with = "no_password"
        )]
        password_user: Option<String>,

        /// File holding the basic-auth password ('-' for stdin; needs --password-user)
        #[arg(
            long,
            value_name = "FILE",
            requires = "password_user",
            conflicts_with = "no_password"
        )]
        password_file: Option<PathBuf>,

        /// Remove password protection
        #[arg(long)]
        no_password: bool,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Delete a site; its name becomes available to others
    #[command(alias = "delete")]
    Rm {
        name: String,

        /// Confirm the deletion
        #[arg(long, short = 'y')]
        yes: bool,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// List a site's deployments (newest first)
    Deploys {
        name: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Deploy the current main again (exit 1 if the deploy fails)
    Redeploy {
        name: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Make an earlier deployment live again
    ///
    /// Make an earlier deployment live again (default: the previous one)
    Rollback {
        name: String,

        /// Deployment ID to roll back to
        #[arg(long, value_name = "ID")]
        to: Option<i64>,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Check whether a site name is available (exit 1 if it is not)
    Check {
        name: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Serve a site on your own domains
    #[command(subcommand)]
    Domain(SiteDomainCommands),
}

#[derive(Subcommand)]
pub(super) enum SiteDomainCommands {
    /// List a site's domains and their DNS records
    List {
        /// The site
        site: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Add a domain to a site and print the DNS records to create
    Add {
        /// The site
        site: String,

        /// A domain you own, like www.example.com
        domain: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Verify a domain's TXT record and start serving
    ///
    /// Check a domain's TXT record and start serving the site there (exit 1 if
    /// the record isn't found yet)
    Verify {
        /// The site
        site: String,

        /// The domain to verify
        domain: String,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Make a verified domain the site's main address
    ///
    /// Its other addresses redirect there
    Primary {
        /// The site
        site: String,

        /// The domain
        domain: String,

        /// Stop redirecting to this domain
        #[arg(long)]
        off: bool,

        #[command(flatten)]
        target: SiteTargetArgs,
    },

    /// Remove a domain from a site
    #[command(alias = "remove")]
    Rm {
        /// The site
        site: String,

        /// The domain to remove
        domain: String,

        /// Confirm the removal
        #[arg(long, short = 'y')]
        yes: bool,

        #[command(flatten)]
        target: SiteTargetArgs,
    },
}

impl SiteCommands {
    #[cfg(test)]
    pub(super) fn target(&self) -> &SiteTargetArgs {
        match self {
            SiteCommands::List { target, .. }
            | SiteCommands::Create { target, .. }
            | SiteCommands::Show { target, .. }
            | SiteCommands::Rename { target, .. }
            | SiteCommands::Set { target, .. }
            | SiteCommands::Rm { target, .. }
            | SiteCommands::Deploys { target, .. }
            | SiteCommands::Redeploy { target, .. }
            | SiteCommands::Rollback { target, .. }
            | SiteCommands::Check { target, .. } => target,
            SiteCommands::Domain(d) => match d {
                SiteDomainCommands::List { target, .. }
                | SiteDomainCommands::Add { target, .. }
                | SiteDomainCommands::Verify { target, .. }
                | SiteDomainCommands::Primary { target, .. }
                | SiteDomainCommands::Rm { target, .. } => target,
            },
        }
    }

    /// Split into the shared target flags and the action `commands::site`
    /// runs. Reads `--password-file` here, so a bad file is a usage error.
    pub(super) fn into_action(
        self,
    ) -> oak_core::Result<(SiteTargetArgs, commands::site::SiteAction, bool)> {
        use commands::site::{PasswordChange, SiteAction};
        let mut confirmed = true;
        let (target, action) = match self {
            SiteCommands::List { org, target } => (target, SiteAction::List { org }),
            SiteCommands::Create {
                name,
                dir,
                spa,
                target,
            } => (target, SiteAction::Create { name, dir, spa }),
            SiteCommands::Show { name, target } => (target, SiteAction::Show { name }),
            SiteCommands::Rename {
                name,
                new_name,
                target,
            } => (target, SiteAction::Rename { name, new_name }),
            SiteCommands::Set {
                name,
                dir,
                spa,
                no_spa,
                password_user,
                password_file,
                no_password,
                target,
            } => {
                let spa = match (spa, no_spa) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                };
                let password = match (password_user, password_file) {
                    (Some(username), Some(file)) => Some(PasswordChange::Set {
                        username,
                        password: commands::site::read_password_file(&file)?,
                    }),
                    _ if no_password => Some(PasswordChange::Remove),
                    _ => None,
                };
                (
                    target,
                    SiteAction::Set {
                        name,
                        dir,
                        spa,
                        password,
                    },
                )
            }
            SiteCommands::Rm { name, yes, target } => {
                confirmed = yes;
                (target, SiteAction::Rm { name })
            }
            SiteCommands::Deploys { name, target } => (target, SiteAction::Deploys { name }),
            SiteCommands::Redeploy { name, target } => (target, SiteAction::Redeploy { name }),
            SiteCommands::Rollback { name, to, target } => {
                (target, SiteAction::Rollback { name, to })
            }
            SiteCommands::Check { name, target } => (target, SiteAction::Check { name }),
            SiteCommands::Domain(d) => match d {
                SiteDomainCommands::List { site, target } => {
                    (target, SiteAction::DomainList { name: site })
                }
                SiteDomainCommands::Add {
                    site,
                    domain,
                    target,
                } => (target, SiteAction::DomainAdd { name: site, domain }),
                SiteDomainCommands::Verify {
                    site,
                    domain,
                    target,
                } => (target, SiteAction::DomainVerify { name: site, domain }),
                SiteDomainCommands::Primary {
                    site,
                    domain,
                    off,
                    target,
                } => (
                    target,
                    SiteAction::DomainPrimary {
                        name: site,
                        domain,
                        primary: !off,
                    },
                ),
                SiteDomainCommands::Rm {
                    site,
                    domain,
                    yes,
                    target,
                } => {
                    confirmed = yes;
                    (target, SiteAction::DomainRm { name: site, domain })
                }
            },
        };
        Ok((target, action, confirmed))
    }
}

#[derive(Subcommand)]
pub(super) enum MarkCommands {
    /// List the repo's marks and how far behind main each is
    List,
    /// Show a mark: its commit, owners, requirements, last move
    Show {
        /// The mark's name.
        mark: String,
    },
    /// Create a mark at a commit on main (repo admins)
    ///
    /// Create a mark. It starts at `--commit` (main's head by default).
    /// Without `--owner-group` anyone who can write the repo moves it.
    Create {
        /// The name: lowercase letters, digits, `.`, `_` and `-`, such as
        /// `server-good` (`<area>-<state>` reads well).
        name: String,
        /// Where it starts: `main`, a full hash or a unique prefix.
        #[arg(long)]
        commit: Option<String>,
        /// The group whose members move it (org agents on it count).
        #[arg(long, value_name = "GROUP")]
        owner_group: Option<String>,
        /// Point into this root instead of main.
        #[arg(long)]
        root: Option<String>,
        /// Marks that must be at or past a commit before this one moves
        /// there. Repeatable or comma-separated.
        #[arg(long, value_delimiter = ',', value_name = "MARK")]
        requires: Vec<String>,
        /// People who may move it; then only they can (repeatable).
        #[arg(long = "approver", value_name = "USER")]
        approvers: Vec<String>,
        /// Let its owners roll it back, not only repo admins.
        #[arg(long)]
        allow_group_rollback: bool,
        /// A line saying what the mark means.
        #[arg(long)]
        description: Option<String>,
        /// An org agent that follows it: gets its moves' grants and
        /// reports on them, but can't move it (repeatable).
        #[arg(long = "follower", value_name = "AGENT")]
        followers: Vec<String>,
    },
    /// Move a mark forward to a commit on main
    ///
    /// Move a mark to COMMIT, which must be its current commit or after it
    /// on main. Refused while a mark it requires is behind COMMIT.
    Move {
        /// The mark's name.
        mark: String,
        /// `main`, a full hash or a unique prefix.
        commit: String,
        /// Why, shown in its log.
        #[arg(long)]
        note: Option<String>,
    },
    /// Move a mark back to an earlier commit
    ///
    /// Move a mark back to COMMIT, before its current one. Takes a repo
    /// admin, or one of its owners when the mark allows group rollback.
    Rollback {
        /// The mark's name.
        mark: String,
        /// A full hash or a unique prefix.
        commit: String,
        /// Why, shown in its log.
        #[arg(long)]
        note: Option<String>,
    },
    /// Show a mark's moves, newest first, with their reports
    Log {
        /// The mark's name.
        mark: String,
        /// How many moves.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Delete a mark and its moves (repo admins)
    Delete {
        /// The mark's name.
        mark: String,
        /// Don't ask.
        #[arg(long)]
        yes: bool,
    },
    /// File a report on a commit (an alias of `oak report`)
    #[command(hide = true)]
    Report {
        /// `main`, a full hash or a unique prefix.
        commit: String,
        /// What it is about.
        #[arg(long)]
        name: String,
        /// The report. Over 64 KiB, its start and end are kept.
        #[arg(
            long,
            conflicts_with = "body_file",
            required_unless_present = "body_file"
        )]
        body: Option<String>,
        /// Read the report from a file (`-` for stdin).
        #[arg(long, value_name = "FILE")]
        body_file: Option<PathBuf>,
        /// A link to more.
        #[arg(long)]
        url: Option<String>,
        /// The mark move it is about.
        #[arg(long)]
        move_id: Option<i64>,
    },
    /// Wait for a mark to move and run a script for each move
    ///
    /// Wait for MARK to move, check the commit out into a work dir (a clone
    /// of the repo, made the first time), and run the script there with
    /// OAK_COMMIT, OAK_MARK, OAK_MOVE_ID, OAK_REPO_ID and the move's signed
    /// grant (OAK_GRANT, OAK_GRANT_SIGNATURE). Files a report on the commit
    /// under the mark's name (see `oak report`): "deploying", then "live" or
    /// "failed" with the end of the output. One move at a time; when
    /// several land meanwhile, only the newest runs. The last move handled is
    /// kept in a state file, so a restart picks up where it stopped.
    Follow {
        /// The mark's name.
        mark: String,
        /// The script to run, relative to the work dir or absolute (its
        /// arguments go after `--`).
        #[arg(long, value_name = "SCRIPT")]
        run: String,
        /// Arguments for the script.
        #[arg(last = true, value_name = "ARGS")]
        script_args: Vec<String>,
        /// The work dir (default `./oak-mark-<MARK>`).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Where to remember the last move (default `<DIR>.follow.json`).
        #[arg(long, value_name = "FILE")]
        state_file: Option<PathBuf>,
        /// Handle at most one move, then exit (0 when none came).
        #[arg(long)]
        once: bool,
        /// Run the script in the work dir without checking anything out.
        #[arg(long)]
        no_checkout: bool,
        /// Run the script even when the server sends no grant (when this
        /// credential isn't one of the mark's followers). Without it that is
        /// an error.
        #[arg(long)]
        no_grant: bool,
        /// The name reports go under (default: the mark's).
        #[arg(long)]
        report_name: Option<String>,
        /// Seconds each request waits for a move.
        #[arg(long, default_value_t = 50, hide = true)]
        wait: u64,
    },
}

#[derive(Subcommand)]
pub(super) enum RetentionCommands {
    /// List the rules and what they have expired (the default).
    List,
    /// Create or change a directory's retention rule
    ///
    /// Create or change the rule for a directory (`/` or `""` for the whole
    /// repo). The deepest matching rule wins.
    Set {
        /// The directory.
        path: String,
        /// How many versions of each file keep their content (at least 1).
        #[arg(long, value_name = "N")]
        keep: i64,
    },
    /// Remove a directory's retention rule
    ///
    /// Remove the rule for a directory. Versions it expired come back unless
    /// their storage has been reclaimed.
    Remove {
        /// The directory.
        path: String,
    },
}

#[derive(Subcommand)]
pub(super) enum SparseCommands {
    /// Show the active cone (default when no subcommand is given).
    #[command(alias = "show")]
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Replace the sparse checkout's paths
    ///
    /// Replace the cone with exactly these path prefixes, then re-sync the
    /// working tree. Repeatable or comma-separated.
    Set {
        /// Path prefixes to scope the checkout to (e.g. `src docs/api`).
        #[arg(value_name = "PREFIX", required = true)]
        paths: Vec<String>,

        /// Emit the resulting checkout scope as JSON (progress on stderr).
        #[arg(long)]
        json: bool,
    },

    /// Add paths to the sparse checkout
    ///
    /// Add path prefixes to the existing cone, then re-sync the working tree,
    /// downloading the files that enter it. Refuses (changing nothing) while
    /// uncommitted or untracked files sit in the old or new cone.
    Add {
        /// Path prefixes to include (e.g. `libs/shared`).
        #[arg(value_name = "PREFIX", required = true)]
        paths: Vec<String>,

        /// Emit the resulting checkout scope as JSON (progress on stderr).
        #[arg(long)]
        json: bool,
    },

    /// Remove paths from the sparse checkout
    ///
    /// Remove path prefixes from the cone, then re-sync the working tree,
    /// deleting the files that leave it. Refuses (changing nothing) while
    /// uncommitted or untracked files sit in the old cone.
    Remove {
        /// Path prefixes to drop (e.g. `libs/shared`).
        #[arg(value_name = "PREFIX", required = true)]
        paths: Vec<String>,

        /// Emit the resulting checkout scope as JSON (progress on stderr).
        #[arg(long)]
        json: bool,
    },

    /// Return to a full checkout
    ///
    /// Drop the cone and return to a full checkout, downloading every file that
    /// was outside it. Refuses (changing nothing) while uncommitted or
    /// untracked files sit anywhere in the working tree.
    Disable {
        /// Emit the resulting checkout scope as JSON (progress on stderr).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum GroupCommands {
    /// List the org's groups as a tree
    ///
    /// Every group in the org, as a tree, with how many people and repos.
    List,
    /// One group: its parent, maintainers, members and repos.
    Show {
        /// Group name (e.g. `web`).
        slug: String,
    },
    /// Create a group
    ///
    /// A top-level group needs an org owner or admin; a sub-group needs a
    /// maintainer of the group above it.
    Create {
        /// Group name (e.g. `web`).
        slug: String,
        /// Make it a sub-group of this group.
        #[arg(long, value_name = "GROUP")]
        parent: Option<String>,
        /// What the group is for.
        #[arg(long)]
        description: Option<String>,
    },
    /// Add someone to a group
    ///
    /// Add someone in the org to a group (or change whether they maintain it).
    /// `oak group add members USER` adds them to the org itself.
    Add {
        /// Group name.
        slug: String,
        /// Username.
        username: String,
        /// Make them a maintainer: they run this group and its sub-groups.
        #[arg(long)]
        maintainer: bool,
    },
    /// Remove someone from a group
    ///
    /// `oak group remove members USER` takes them out of the org (and every
    /// group); anyone may remove themselves.
    Remove {
        /// Group name.
        slug: String,
        /// Username.
        username: String,
    },
    /// Add someone to a group, inviting them to the org
    ///
    /// Put someone in a group, adding them to the org first if they aren't in
    /// it yet (that part takes an org owner or admin).
    Invite {
        /// Group name.
        slug: String,
        /// Username.
        username: String,
        /// Make them a maintainer.
        #[arg(long)]
        maintainer: bool,
    },
    /// Give a group read or write access to a repo
    ///
    /// Give a group Read or Write on a repo (org owners and admins). `oak group
    /// grant public REPO` makes the repo public.
    Grant {
        /// Group name.
        slug: String,
        /// The repo, as REPO or ORG/REPO.
        repo: String,
        /// read or write.
        #[arg(long, default_value = "read", value_name = "read|write")]
        role: String,
    },
    /// Take away a group's access to a repo
    ///
    /// `oak group revoke public REPO` makes the repo private.
    Revoke {
        /// Group name.
        slug: String,
        /// The repo, as REPO or ORG/REPO.
        repo: String,
    },
    /// Put the org's agents on groups
    ///
    /// The org's own agents (non-human accounts) on groups. An agent reaches what the
    /// groups it's on reach, never its creator's access.
    Agent {
        #[command(subcommand)]
        action: GroupAgentCommands,
    },
    /// Work as a group in this checkout
    ///
    /// Make this checkout work as a group: you see and change exactly what the
    /// group can, until `--clear`. With no name, says which it is. Uses a
    /// 30-day key whose grant is the group, for this checkout only.
    Switch {
        /// Group name. `-` is the same as --clear.
        slug: Option<String>,
        /// Go back to your own access.
        #[arg(long, conflicts_with = "slug")]
        clear: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum GroupAgentCommands {
    /// List the org's agents and their groups
    ///
    /// The org's agents and the groups each is on (org owners and admins).
    List,
    /// Put an org agent on a group
    ///
    /// Put one of the org's agents on a group. It then reaches what the group
    /// reaches. Takes being a maintainer of the group (or an org owner or
    /// admin); built-in groups don't take agents.
    Add {
        /// Group name (e.g. `web`).
        slug: String,
        /// Agent name (e.g. `deploy-bot`).
        agent: String,
    },
    /// Take an agent off a group.
    Remove {
        /// Group name.
        slug: String,
        /// Agent name.
        agent: String,
    },
}
