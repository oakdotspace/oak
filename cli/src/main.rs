use oak_cli::commands;
use oak_cli::output;

use std::io::Read;
use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Styles};
use clap::{Parser, Subcommand};

/// Green-on-default styling for clap's auto-generated help, so
/// `oak <command> --help` matches the hand-rolled `oak --help` banner:
/// bold-white section headers, green command/flag literals and value
/// placeholders. clap only emits these when its `color` feature is on and
/// the stream is a TTY (ColorChoice::Auto), so piped output stays clean.
fn help_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::White.on_default().bold())
        .usage(AnsiColor::White.on_default().bold())
        .literal(AnsiColor::Green.on_default())
        .placeholder(AnsiColor::Green.on_default())
}

const EXIT_CODE_HELP: &str = "\
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
struct Cli {
    /// Print verbose timing info for each phase (also via `OAK_VERBOSE=1`).
    /// Note: must come before the subcommand, e.g. `oak --verbose commit`.
    #[arg(long)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a repository in the current or given directory
    Init {
        /// Directory to initialize (defaults to current directory)
        path: Option<PathBuf>,

        /// Use git as the storage backend: create a plain git repository
        /// that oak reads and writes directly, so it can be hosted on GitHub
        /// or any git server (`oak push` / `oak pull` go through `git`).
        #[arg(long)]
        git: bool,

        /// Emit one JSON receipt on stdout (never prompts): the repository
        /// path, the branch it starts on, and commits imported from git.
        #[arg(long, conflicts_with = "git")]
        json: bool,
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
    /// Stacks land bottom-up: merge the bottom branch, and the server restacks
    /// the rest onto main.
    ///
    /// When the parent is main (the normal case) this is a remote mutation,
    /// not a local-only merge: `oak merge` first pushes BRANCH (default: the
    /// current branch), then the Oak server squash-merges it into main,
    /// advancing the remote main and closing the branch; the closed state is
    /// mirrored locally. There is no later `oak push` step. The branch
    /// description becomes the squash commit message. CI doesn't hold the
    /// merge back: it runs on main after the merge (`oak ci status` shows
    /// each service there). Path rules that need approval can refuse the
    /// merge until an owner approves. A branch stacked on another feature branch can't
    /// land directly: merge the bottom of the stack first and the server
    /// restacks the rest onto main (`--into-parent` folds it into its parent
    /// locally instead). Use `--dry-run --json` for a read-only prediction,
    /// certified against the remote main head, first.
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

        /// For a branch stacked on another feature branch: merge it into
        /// that parent branch locally (folding it in) instead of refusing.
        /// Without this, stacks land bottom-up through `oak merge` on the
        /// bottom branch.
        #[arg(long)]
        into_parent: bool,

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

        /// Accepted so older scripts keep working; it changes nothing. CI
        /// runs on main after the merge and never holds one back, so there
        /// is nothing to wait for. (It still takes an optional number of
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

        /// With -c: stack the new branch on the current branch instead of
        /// main. It starts at the current branch's head, merges back into it,
        /// and is restacked onto main once the current branch lands.
        #[arg(long, requires = "create", conflicts_with = "parent")]
        stack: bool,

        /// With -c: stack the new branch on BRANCH (an open feature branch)
        /// instead of main. `--parent main` is the default, unstacked branch.
        #[arg(long, value_name = "BRANCH", requires = "create")]
        parent: Option<String>,

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

    /// Start a task: a new branch, described, pushed, watched
    ///
    /// Creates a branch named after SUMMARY from latest main (`oak switch -c`
    /// semantics: dirty files are carried over), sets its description to
    /// SUMMARY, pushes it, and starts `oak watch`.
    ///
    /// Push is skipped by `--no-push` or `watch.auto_push = false`, and watch
    /// by `--no-watch` or `watch.enabled = false` (see `oak config`).
    // Args live in their own struct so clap builds them in a separate
    // function: `Commands::augment_subcommands` is one huge stack frame in
    // debug builds, and the parse tests run on 2 MiB test threads.
    Start(StartArgs),

    /// Keep this branch merged with main and pushed
    ///
    /// Keep this checkout's task branch live: a background daemon merges main
    /// in as it moves (checkpointing dirty work first; conflicts are ordinary
    /// `oak pull` conflicts and pause it), pushes every new commit, and marks
    /// the branch Active on the server. With no subcommand, starts it.
    #[command(args_conflicts_with_subcommands = true)]
    Watch {
        #[command(subcommand)]
        action: Option<WatchCommands>,

        /// Run in this process instead of detaching (logs to stderr).
        #[arg(long)]
        foreground: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Read or change agent-sync settings (watch.*)
    ///
    /// Read or change agent-sync settings (watch.enabled, watch.auto_pull,
    /// watch.auto_push, watch.share_working). Precedence: env (OAK_WATCH,
    /// OAK_WATCH_AUTO_PULL, OAK_WATCH_AUTO_PUSH) > repo config > global
    /// ~/.oak/config.json > your oak.space preference > default (true;
    /// watch.share_working is off by default, and sends uncommitted text files
    /// to the branch page).
    Config {
        #[command(subcommand)]
        action: ConfigCommands,
    },

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

    /// Detach HEAD at a commit (full hash or unique prefix)
    Checkout {
        /// Commit hash, or a unique short prefix (≥ 4 hex chars)
        reference: String,
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

    /// Show repository and branch metadata
    Info {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Print the current HEAD commit hash, bare, for scripts
    Hash,

    /// Compatibility alias for `oak hash`.
    RevParse {
        /// Print a short hash, compatible with git rev-parse --short HEAD
        #[arg(long)]
        short: bool,

        /// Revision to resolve. Oak currently supports HEAD here.
        rev: String,
    },

    /// Local database maintenance (compaction, etc.)
    Maintenance {
        #[command(subcommand)]
        action: MaintenanceCommands,
    },

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

    /// Update this branch, then merge in its parent
    ///
    /// Bring the local clone fully up to date: fetch new commits on the current
    /// branch from the remote, then merge in any changes from the parent
    /// branch.
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

    /// Check remote content integrity, commits to bytes
    ///
    /// Exits 0 when verified, 6 when the proof found a problem, and 8 when
    /// the proof ran out of budget (inconclusive — narrow it with --depth).
    /// `--json` adds `outcome`: `verified`, `failed`, or `inconclusive`.
    Doctor {
        /// Repository in ORG/REPO form
        #[arg(long, value_name = "ORG/REPO")]
        repo: String,

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

    /// Inspect committed files without hydrating them
    File {
        #[command(subcommand)]
        action: FileCommands,
    },

    /// Inspect local refs from one pinned snapshot (read-only)
    Refs {
        #[command(subcommand)]
        action: RefsCommands,
    },

    /// Inspect a commit's whole tree (read-only)
    Tree {
        #[command(subcommand)]
        action: TreeCommands,
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

    /// Inspect privileged blob integrity evidence
    Blob {
        #[command(subcommand)]
        action: BlobCommands,
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

    /// Keep only the newest versions of files in a directory
    ///
    /// Older versions keep their commits and history, but the server stops
    /// serving their content and reclaims the storage. Content a branch head, a
    /// release, or a file outside the rule still uses is always kept. Rules
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
    /// List a repo's ownership boundaries: each directory with its owner team,
    /// whether a change there needs that team's approval (`required`,
    /// `advisory` or `off`), and who may read and write it. Boundaries live on
    /// the server; ones over directories you can't read aren't listed.
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
    /// (same as `oak file inspect --remote`). Read-only.
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

    /// Create a zip archive of the current directory
    Archive {
        /// Output file path (defaults to <directory_name>.zip)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Inspect and control server-side CI runs for this repo.
    ///
    /// CI runs on main, after a branch merges, never on branch pushes, and
    /// it never holds a merge back: test locally, merge once you're up to
    /// date with main, and fix main if a service breaks. Each workflow file
    /// is a service that passes or fails on its own; `oak ci status` shows
    /// each one on main. Channels publish the newest commit that is good
    /// for the services they wait for (`oak ci publish` authorizes one
    /// publish by hand). The other subcommands list runs, read step logs,
    /// and re-dispatch a run that failed for infra reasons.
    Ci {
        #[command(subcommand)]
        action: CiCommands,
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
    /// service accounts are refused. With no --path, approves every touched
    /// boundary you may. Run from a checkout, or name the target with --repo.
    Approve {
        /// The branch (defaults to the current one).
        #[arg(value_name = "BRANCH", conflicts_with = "branch_flag")]
        branch: Option<String>,

        /// The branch, as `--repo` targets take it.
        #[arg(long = "branch", value_name = "NAME")]
        branch_flag: Option<String>,

        /// Approve one boundary, named by its directory.
        #[arg(long = "path", value_name = "DIR")]
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
    /// Export this oak repository's history into a fresh git repository (or,
    /// with --tree-only --at HASH, materialize one verified tree).
    ///
    /// Replays the current branch's linear ancestry as git commits, preserving
    /// original author + timestamp. Useful as the documented escape hatch:
    /// move your code off oak whenever you want.
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

        /// Write into the destination even if it already contains files.
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
    /// upgrades and the daily update notice. On macOS, an installed Oak Mount
    /// app is updated along with the CLI.
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
        /// Also offer the tools that commit, set the description, and push
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

    /// Mount remote repos as lazy virtual filesystems
    ///
    /// Files are downloaded lazily on access — useful for very large repos
    /// where a full clone is impractical. Writes happen on a virtual branch
    /// that lives only locally until you push it. Each mount runs as a
    /// detached background daemon, so the command returns once the mount is
    /// live and hands your terminal back.
    ///
    /// Forms:
    ///   oak mount                       print this help
    ///   oak mount <repo>                mount your org's repo at ./<repo>
    ///   oak mount <org>/<repo>          mount that repo at ./<repo>
    ///   oak mount <org>/<repo> <dest>   mount that repo at <dest>
    ///
    /// By default a mount starts a fresh virtual branch off the repo's trunk.
    /// Pass `--branch <name>` to mount an *existing* remote branch instead —
    /// its history and files become the mount's, and `oak pull` / `oak commit`
    /// / `oak push` inside the mount continue that branch (e.g. to resolve its
    /// merge conflicts locally without a full clone).
    ///
    /// Inside a mount, the normal commands (`oak status`, `oak commit`,
    /// `oak push`, …) operate on the virtual branch. Use `oak mount list` to
    /// see active mounts and `oak mount end` to tear them down.
    ///
    /// macOS uses Apple FSKit (no kernel extension); it needs macOS 26+ and
    /// the signed "Oak Mount" app installed and enabled. Linux uses FUSE via
    /// the `fusermount3` helper from the `fuse3` package. Windows uses the
    /// Windows Projected File System (ProjFS) optional feature.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[command(verbatim_doc_comment)]
    Mount {
        #[command(subcommand)]
        action: Option<MountCommands>,

        /// What to mount: `<repo>` (in your org) or `<org>/<repo>`.
        /// Omit to print this help.
        #[arg(value_name = "ORG/REPO")]
        spec: Option<String>,

        /// Destination directory for a single `<org>/<repo>` mount.
        /// Defaults to `./<repo>`.
        dest: Option<PathBuf>,

        /// Mount an existing remote branch instead of starting a fresh
        /// virtual branch off the trunk. The mount's history and files are
        /// the branch's, and commits/pushes inside the mount continue it.
        #[arg(short, long)]
        branch: Option<String>,

        /// Mount only this directory (repeatable or comma-separated), e.g.
        /// `oak mount acme/mono --path apps/web --path libs/ui ./task`. Needs
        /// a server that serves partial checkouts of the repo; the other
        /// directories aren't in the mount at all.
        #[arg(long = "path", value_name = "DIR")]
        paths: Vec<String>,

        /// Remote server URL.
        #[arg(short, long)]
        remote: Option<String>,
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
enum FileCommands {
    /// Verify one committed file's bytes
    ///
    /// Verify one committed file's logical bytes in a pinned local SQLite
    /// snapshot (or, with --remote, read it from one exact remote commit,
    /// hash-verified)
    Inspect {
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
}

#[derive(Subcommand)]
enum RefsCommands {
    /// Check local refs for disagreements (read-only)
    ///
    /// Read current branch, branch heads, parent inheritance, legacy HEAD and
    /// the effective HEAD from ONE pinned local SQLite snapshot, and flag any
    /// disagreement between them. Read-only; no network.
    Inspect {
        /// List at most this many branches (sorted by name)
        #[arg(long, default_value_t = oak_core::sqlite::refs_inspect::DEFAULT_REFS_MAX_BRANCHES,
              value_parser = clap::value_parser!(u64).range(1..=100_000))]
        max_branches: u64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum TreeCommands {
    /// List a commit's paths, sizes, and hashes
    ///
    /// Verify and list one pinned commit's complete path set: modes, logical
    /// sizes, BLAKE3 blob ids and SHA-256 of verified bytes. Bounded; a budget
    /// stop is reported as explicit truncation. Local only.
    Inspect {
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
}

#[derive(Subcommand)]
enum ChangeCommands {
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
enum BlobCommands {
    /// Inspect one blob's metadata, chunks, and paths
    ///
    /// Prove one blob's metadata, mapping, chunks, objects, and affected paths
    Info {
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
}

#[derive(Subcommand)]
enum BranchCommands {
    /// Preview landing several sibling branches in order
    ///
    /// Preview an ordered train of independent sibling branches without
    /// publishing.
    Train {
        /// Explicit source branches, in application order (at most 32).
        #[arg(required = true)]
        names: Vec<String>,
        /// Pin this initial target branch.
        #[arg(long, default_value = "main")]
        against: String,
        /// Pin remote heads and recheck them after bounded preparation.
        #[arg(long)]
        remote: bool,
        /// Emit one machine-readable prediction or stopped-preview document.
        #[arg(long)]
        json: bool,
    },
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

    /// Close a branch (same as `oak close`)
    ///
    /// Defaults to the current branch. Takes exactly the arguments of `oak
    /// close`, and runs it: see `oak close --help` for closing several remote
    /// branches pinned to exact heads.
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
}

impl Commands {
    /// Rewrite `oak branch close ...` into the `oak close ...` it aliases, so
    /// dispatch, JSON error envelopes and exit codes are the same for both.
    /// `--show-current` is left alone for the branch handler to refuse.
    fn resolve_aliases(self) -> Self {
        match self {
            Commands::Branch {
                json: branch_json,
                show_current: false,
                action:
                    Some(BranchCommands::Close {
                        names,
                        remote,
                        reason,
                        json,
                    }),
            } => Commands::Close {
                names,
                remote,
                reason,
                json: json || branch_json,
            },
            command => command,
        }
    }
}

#[derive(Subcommand)]
enum AgentCommands {
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
enum AgentPublicationCommands {
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
enum CiCommands {
    /// Trigger CI at an exact commit, safely retryable
    ///
    /// Atomically trigger ordinary CI at an exact head with a durable retry
    /// key. Requires server ordinary_trigger_v1 support; never falls back to
    /// legacy dispatch.
    Trigger {
        /// Full lowercase expected commit hash (never inferred from a moving head).
        #[arg(long, value_name = "HASH")]
        expected_commit: String,
        /// Stable key for this request; reuse unchanged when retrying.
        #[arg(long, value_name = "KEY")]
        idempotency_key: String,
        /// Branch to compare atomically (defaults to current branch).
        #[arg(long)]
        branch: Option<String>,
        /// Ordinary workflow file stem; omitted selects all ordinary workflows.
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List recent CI runs for this repo (newest first).
    Runs {
        /// Maximum number of runs to show.
        #[arg(long, default_value_t = oak_cli::commands::ci::DEFAULT_RUNS_LIMIT)]
        limit: usize,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Show each CI service's state on main
    ///
    /// Each service (workflow file) on main: passing, failing since a commit,
    /// or running. Exit codes: 0 every service passing; 1 a service failing (or
    /// no runs on main); 3 a service still running and none failing (retry
    /// later) — distinct so scripts can branch without parsing output. `--head`
    /// reports the current checkout head instead, and `--run` one exact run.
    Status {
        /// Report the newest run for the current checkout head instead of
        /// the services on main (branch pushes don't run CI, so a branch
        /// head usually has none).
        #[arg(long, conflicts_with = "run")]
        head: bool,

        /// Inspect this exact run instead of the services on main.
        #[arg(long)]
        run: Option<u64>,

        /// Require the observed run to match this full commit hash.
        #[arg(long, requires = "run")]
        commit: Option<String>,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Wait for CI runs to finish (read-only)
    ///
    /// Wait for exact CI run ids to reach terminal states without mutating
    /// repository or CI state. Exit codes: 0 success, 1 failure, 3 timeout (or,
    /// with --current, no run dispatched yet: state `dispatch_pending`), 6
    /// server/read error. Transient read failures (connection and body-read
    /// errors, HTTP 408, 429, 500, 502, 503, 504) are retried within --timeout;
    /// 501 and other statuses are conclusive. Exit 6 with a `category=` then
    /// means the latest read of a requested run was still failing at the
    /// deadline, so its state is unknown.
    Wait {
        /// Run ids to wait for (from `oak ci runs`, `status`, or `rerun`).
        #[arg(required_unless_present = "current", num_args = 1..)]
        run_ids: Vec<u64>,

        /// Wait for the current branch head instead of run ids: resolve the
        /// head once, bind to the newest run per workflow for exactly that
        /// commit on this branch (waiting up to --dispatch-timeout; then
        /// same-commit runs from other branches, labelled), and fence the
        /// wait with that commit. Never binds to an older head.
        #[arg(long, conflicts_with_all = ["run_ids", "commit"])]
        current: bool,

        /// With --current: seconds to wait for a run to be dispatched for the
        /// head before reporting `dispatch_pending` (bounded by --timeout;
        /// default 120).
        #[arg(long, value_name = "SECS", requires = "current")]
        dispatch_timeout: Option<u64>,

        /// Require every observed run to belong to this exact commit hash.
        #[arg(long, value_name = "HASH")]
        commit: Option<String>,

        /// Maximum number of seconds to wait; zero performs one poll with a
        /// one-second network-I/O ceiling.
        #[arg(long, default_value_t = 1800)]
        timeout: u64,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,

        /// Return bounded per-run metadata without logs (requires --json).
        /// Full legacy run details may still be downloaded from older servers.
        #[arg(long, requires = "json")]
        summary: bool,

        /// Print bounded step-transition progress on stderr while waiting
        /// (from run/step metadata only; no logs, no test counts). Stdout
        /// keeps the single final document.
        #[arg(long)]
        progress: bool,

        /// Emit progress as JSON lines on stderr (implies --progress).
        #[arg(long)]
        events: bool,
    },

    /// Show a run's step-by-step logs.
    Logs {
        /// Run id (from `oak ci runs` / `oak ci status`).
        run_id: u64,

        /// Show step metadata without logs or script bodies.
        #[arg(long, conflicts_with_all = ["failed", "max_bytes"])]
        summary: bool,

        /// Include only failed steps (with a default 64 KiB total log budget).
        #[arg(long)]
        failed: bool,

        /// Maximum total UTF-8 log bytes returned; truncation is explicit.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<usize>,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Rerun a run (legacy; the commit isn't guaranteed)
    ///
    /// Request a legacy manual rerun. Older servers may use the current branch
    /// head; this is not exact-commit or idempotent dispatch. Reports every
    /// returned run id.
    Rerun {
        /// Run id to re-run (from `oak ci runs` / `oak ci status`).
        run_id: u64,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Cancel one exact run, or superseded runs
    ///
    /// Cancel an exact ordinary push/merge run. Manual, protected, release,
    /// terminal, moved, and unknown runs are rejected before mutation. With
    /// --superseded, list (dry run) or, with --yes, cancel older in-flight push
    /// runs on the current branch whose commit is not the head.
    Cancel {
        /// Exact run id to cancel (never inferred from a branch or latest run).
        #[arg(required_unless_present = "superseded")]
        run_id: Option<u64>,

        /// Full lowercase commit hash that the run must match.
        #[arg(long, value_name = "HASH", required_unless_present = "superseded")]
        commit: Option<String>,

        /// Target in-flight ordinary push runs on the current branch that an
        /// exact newer published head superseded. Dry run unless --yes.
        #[arg(long, conflicts_with_all = ["run_id", "commit"])]
        superseded: bool,

        /// With --superseded: only list what would be cancelled (default).
        #[arg(long, requires = "superseded", conflicts_with = "yes")]
        dry_run: bool,

        /// With --superseded: actually cancel each listed eligible run through
        /// the exact-run cancel path.
        #[arg(long, requires = "superseded")]
        yes: bool,

        /// Emit a minimal machine-readable receipt.
        #[arg(long)]
        json: bool,
    },

    /// Release a run held by path permissions
    ///
    /// Approve a run the server held because the repo's access boundaries keep
    /// its author from reading every path. Only a writer with full read access
    /// can approve, never the run's own author; approvals are audit-logged.
    Approve {
        /// Exact held run id (from `oak ci status` / `oak ci runs`).
        run_id: u64,

        /// Full lowercase commit hash that the run must match.
        #[arg(long, value_name = "HASH")]
        commit: String,

        /// Emit a minimal machine-readable receipt.
        #[arg(long)]
        json: bool,
    },

    /// Publish main's head with one publish workflow
    ///
    /// WORKFLOW is a workflow file set up to publish in the repo's Settings
    /// → CI: its name (`release`), file name (`release.yml`) or path.
    /// Previews the exact values the server would authorize (main's head,
    /// the workflow's blob hash, its settings version), its reviewers,
    /// runner state, and any problems; asks for confirmation (or --yes);
    /// then authorizes exactly those values. The server refuses if any of
    /// them moved since the preview. Exit codes: 0 authorized (or --dry-run
    /// would proceed); 1 not ready, declined, or with --wait the run
    /// failed; 2 confirmation needed but no --yes (no TTY, or --json); 3
    /// --wait timed out; 6 server/auth refusal, including a caller who
    /// cannot approve this workflow.
    #[command(name = "publish", visible_alias = "deploy")]
    Deploy {
        /// The publish workflow: name, file name or path.
        #[arg(value_name = "WORKFLOW", required_unless_present = "environment")]
        target: Option<String>,

        /// Older spelling of WORKFLOW (also --channel).
        #[arg(
            short = 'e',
            long,
            alias = "channel",
            value_name = "NAME",
            conflicts_with = "target",
            hide = true
        )]
        environment: Option<String>,

        /// Older scripts name the workflow here too.
        #[arg(long, value_name = "NAME", hide = true)]
        workflow: Option<String>,

        /// Publishes are from main; kept for older scripts.
        #[arg(long, hide = true)]
        branch: Option<String>,

        /// Republish this older commit (full hash) instead of the head. It
        /// must be on main and have passed Verify.
        #[arg(long, value_name = "HASH")]
        commit: Option<String>,

        /// With --commit, also pause the workflow's auto-publish, so the
        /// next merge or timer doesn't replace the commit you republished.
        #[arg(long, requires = "commit")]
        pause_auto_publish: bool,

        /// Authorize without the interactive confirmation (required without
        /// a TTY and with --json).
        #[arg(long)]
        yes: bool,

        /// Only preview; never authorizes.
        #[arg(long)]
        dry_run: bool,

        /// After authorizing, wait for the run to conclude (the exact-run,
        /// commit-fenced `oak ci wait`) and exit with its status. Takes an
        /// optional timeout in minutes; a bare `--wait` waits up to 30.
        #[arg(
            long,
            value_name = "MINUTES",
            num_args = 0..=1,
            default_missing_value = "30",
            conflicts_with = "dry_run"
        )]
        wait: Option<u64>,

        /// Emit one machine-readable JSON document (preview, authorization,
        /// and with --wait the run's wait summary). Never prompts.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum FeedbackCommands {
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
enum ConflictCommands {
    /// Summarize any in-progress conflict
    ///
    /// Summarize any in-progress merge, pull sync, or mount pull conflict.
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
enum RepoCommands {
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
enum AuthCommands {
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
struct DiffArgs {
    /// Revisions and/or path filters. Up to two leading arguments that
    /// name a branch or commit (unique hash prefix, ≥ 4 hex chars)
    /// select the diff endpoints — `oak diff <branch>` shows the
    /// branch's contribution checkout-free, `oak diff <commit>` compares
    /// it to the working tree, `oak diff <rev> <rev>` diffs two
    /// revisions. Remaining arguments (or everything after `--`) limit
    /// the diff to those files or directories
    paths: Vec<PathBuf>,

    /// Base to compare a branch endpoint against (defaults to the
    /// branch's parent)
    #[arg(long)]
    against: Option<String>,

    /// Diff perspective for branch endpoints: contribution (fork point
    /// vs branch — what the branch did; default for `oak diff
    /// <branch>`), tree (base head vs branch head; default for two
    /// revisions), or net-merge (base vs predicted merge result)
    #[arg(long)]
    mode: Option<String>,

    /// Emit machine-readable JSON summary
    #[arg(long)]
    json: bool,

    /// Return at most N changed-file summaries in --json output
    #[arg(long, requires = "json")]
    changed_files_limit: Option<usize>,

    /// Start --json changed-file summaries at this offset
    #[arg(long, requires = "json", default_value_t = 0)]
    changed_files_offset: usize,

    /// Include unified hunks per file in --json output (progressive
    /// disclosure: summary first, then hunks — scope with path filters
    /// to fetch one file's patch)
    #[arg(long, requires = "json")]
    hunks: bool,

    /// Byte budget across all --hunks patches; files past the budget
    /// carry "patch_omitted": true and can be fetched individually
    #[arg(long, requires = "hunks")]
    max_bytes: Option<usize>,

    /// Print the diff to stdout instead of opening the interactive
    /// browser. Combines with --name-only / --stat, which are already
    /// non-interactive and take precedence over full hunks
    #[arg(long)]
    print: bool,

    /// Show per-file +added/-removed line counts instead of full hunks
    #[arg(long, conflicts_with = "name_only")]
    stat: bool,

    /// Show only changed file paths
    #[arg(long, conflicts_with = "json")]
    name_only: bool,

    /// Diff the whole branch — its commits plus uncommitted changes —
    /// against its fork point on the parent branch, instead of only
    /// uncommitted changes vs HEAD
    #[arg(long)]
    branch: bool,

    /// Exit with code 1 when differences exist and 0 when there are
    /// none (like `git diff --exit-code`), for scripting without
    /// parsing output
    #[arg(long)]
    exit_code: bool,

    /// Number of context lines around hunks in printed/browsed diffs
    #[arg(short = 'U', long = "unified", value_name = "N")]
    unified: Option<usize>,

    /// Mark intra-line changes in printed hunks with [-old-] / {+new+}
    #[arg(long, requires = "print")]
    word_diff: bool,

    /// Treat oversized text files as text instead of showing the
    /// large-file notice. NUL-containing binary files still stay binary.
    #[arg(short = 'a', long = "text")]
    text: bool,

    /// Check added lines for whitespace errors (trailing whitespace,
    /// space before tab in indent, new blank lines at EOF) and
    /// unresolved conflict markers instead of showing the diff. Prints
    /// `path:line: problem`, or with --json one document listing
    /// {path, line, kind}. Exits 1 when problems are found, 0 when clean
    #[arg(
        long,
        conflicts_with_all = ["name_only", "stat", "hunks", "word_diff", "fingerprint", "verify_fingerprint", "exit_code"]
    )]
    check: bool,

    /// Add a `fingerprint` object to the working-tree --json document:
    /// {change_id, base_commit, base_tree, result_tree,
    /// changed_file_count}. `change_id` is the id `oak change capture`
    /// would record for the same tree and paths; computing it writes
    /// nothing and needs no linked remote
    #[arg(long, requires = "json", conflicts_with = "branch")]
    fingerprint: bool,

    /// Recompute the working-tree fingerprint (optionally scoped to
    /// paths) and exit 1 unless its change_id equals ID. Writes nothing
    #[arg(
        long,
        value_name = "ID",
        conflicts_with_all = ["branch", "name_only", "stat", "print", "hunks", "fingerprint"]
    )]
    verify_fingerprint: Option<String>,

    /// Number hunk lines. With --json --hunks, each file with a
    /// patch also gets structured `hunks`:
    /// [{old_start, old_len, new_start, new_len, lines: [{kind,
    /// old_line, new_line, text}]}], bounded by the same --max-bytes
    /// budget. With --print, hunk lines are prefixed with their old/new
    /// line numbers (the output is then for reading, not `apply`)
    #[arg(long, conflicts_with_all = ["check", "stat", "name_only"])]
    line_numbers: bool,

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
    remote: bool,
}

#[derive(clap::Args)]
struct CloneArgs {
    /// Remote server URL
    #[arg(short, long)]
    remote: Option<String>,

    /// Repository spec in `org/repo` form (e.g. `oak/oak`). A bare
    /// `repo` is allowed when logged in — it defaults to your personal
    /// organization (your username as org). Omit to search and pick.
    #[arg(value_name = "ORG/REPO")]
    name: Option<String>,

    /// Destination directory. Defaults to the repo name in the current
    /// directory (git-style — `oak clone oak/foo` clones into `./foo`).
    dest: Option<PathBuf>,

    /// After cloning, switch to this remote branch.
    #[arg(long, value_name = "NAME")]
    branch: Option<String>,

    /// Require the selected branch to be open at this exact full Oak
    /// commit hash. The server verifies the pin before any clone files
    /// are created and revalidates it when serving the snapshot. Requires
    /// --branch NAME, or --detached to pin the default branch (main).
    #[arg(long, value_name = "FULL")]
    expected_head: Option<String>,

    /// Clone only the newest commit on the selected branch (the default
    /// branch when --branch is omitted), like `git clone --depth=1`.
    /// This narrows local history plus preflight/recovery scope; the
    /// selected commit's working tree remains complete. By default `oak
    /// clone` downloads the full reachable history.
    #[arg(long)]
    shallow: bool,

    /// Proceed when the server's bounded metadata preflight is inconclusive
    /// because it exhausted only its history/tree/path or wall-time
    /// budget. Completeness is then not proved; the download stays pinned
    /// to the proof's snapshot and pull still hash-verifies objects.
    /// Corruption findings are never overridden. Without this flag such a
    /// clone exits 8 and recommends the exact `--shallow` retry.
    #[arg(long)]
    allow_unverified_integrity: bool,

    /// Permit a scoped clone when an accessible legacy server lacks the
    /// bounded proof capability. Pull may honor the requested scope and
    /// still verifies hashes; this waives only proof before download.
    #[arg(long)]
    allow_legacy_scope: bool,

    /// Sparse (partial) clone: check out only the files under these path
    /// prefixes, Perforce-style. Repeatable (`--path src --path docs/api`)
    /// or comma-separated. The repo's full tree is still listed, but the
    /// content of out-of-cone files is not downloaded or written to disk —
    /// commits carry those paths forward untouched. Manage the cone later
    /// with `oak sparse`.
    #[arg(long = "path", value_name = "PREFIX")]
    paths: Vec<String>,

    /// Deprecated no-op: full history is now the default. Accepted so
    /// existing `oak clone --full` invocations keep working.
    #[arg(long, hide = true, conflicts_with = "shallow")]
    full: bool,

    /// Emit one bounded machine-readable acquisition receipt on success
    #[arg(long)]
    json: bool,

    /// Read-only acquisition: land on a detached HEAD at the default
    /// branch's head instead of creating a personal branch. `oak commit`
    /// refuses until you run `oak switch -c`. With --expected-head FULL the
    /// default branch must be open at that exact head, and the clone lands
    /// there.
    #[arg(long, conflicts_with = "branch")]
    detached: bool,

    /// Seed content from an existing local checkout of the same repository
    /// (same owner/name and remote origin) instead of downloading it. Only
    /// content chunks are reused, each re-hashed against the server's
    /// descriptors; commits, trees, refs and the head still come from the
    /// server, and no credentials, locks, refs or dirty files are copied.
    /// The receipt's `seed` block reports reuse counts.
    #[arg(long = "from", value_name = "LOCAL_CHECKOUT")]
    from: Option<PathBuf>,

    /// For a git URL: keep the clone git-backed instead of importing its
    /// history into Oak storage. Oak commands then work on the clone's
    /// `.git` directly and `oak push` / `oak pull` talk to the same host
    /// (GitHub, GitLab, ...).
    #[arg(long)]
    git: bool,
}

#[derive(clap::Args)]
struct LogArgs {
    /// Maximum number of commits to show
    #[arg(short = 'n', long)]
    limit: Option<usize>,

    /// Show verbose output including changed files
    #[arg(short, long, conflicts_with = "oneline")]
    verbose: bool,

    /// Show one compact commit per line
    #[arg(long, conflicts_with = "json")]
    oneline: bool,

    /// Emit machine-readable JSON
    #[arg(long)]
    json: bool,

    /// Only show commits touching these paths (files or directories)
    #[arg(value_name = "PATH")]
    paths: Vec<String>,

    /// Only show commits whose changes add or remove occurrences of TERM
    /// (like `git log -S`). -S counts literal occurrences; use -G to match
    /// a regex against changed lines instead
    #[arg(short = 'S', long = "search", value_name = "TERM")]
    search: Option<String>,

    /// Only show commits whose diff has an added or removed line matching
    /// PATTERN (like `git log -G`). -G matches a regex against changed
    /// lines; -S counts literal occurrences
    #[arg(
        short = 'G',
        long = "search-regex",
        value_name = "PATTERN",
        conflicts_with = "search"
    )]
    search_regex: Option<String>,

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
    remote: bool,

    /// With --remote: the branch to walk (default main)
    #[arg(long, requires = "remote")]
    branch: Option<String>,

    /// With --remote: start from this exact full commit hash instead of
    /// the listed branch head (continuation from next_command)
    #[arg(long, requires = "remote")]
    from: Option<String>,
}

#[derive(clap::Args)]
struct StartArgs {
    /// One-line task summary; becomes the branch name (slugified) and
    /// description.
    summary: String,

    /// Branch name to use instead of the slugified summary.
    #[arg(long)]
    name: Option<String>,

    /// Do not push the new branch.
    #[arg(long)]
    no_push: bool,

    /// Do not start `oak watch`.
    #[arg(long)]
    no_watch: bool,

    /// Start from latest main cleanly, discarding working-tree changes.
    #[arg(long)]
    clean: bool,

    /// Emit a machine-readable receipt.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum WatchCommands {
    /// Start the watch daemon for this checkout (idempotent).
    Start {
        /// Run in this process instead of detaching (logs to stderr).
        #[arg(long)]
        foreground: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Stop the watch daemon and mark the branch idle.
    Stop {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show whether watch is running and its state
    ///
    /// Whether watch is running, its branch, last pull/push, any pending
    /// conflict, and the effective settings.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Claude Code hook entrypoint
    ///
    /// Claude Code hook entrypoint (reads the hook JSON on stdin): EVENT is
    /// SessionStart, UserPromptSubmit, PostToolUse, or SessionEnd.
    Hook {
        /// Hook event name.
        event: String,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Print a setting's effective value
    ///
    /// Print a key's effective value (or, with --global, the global file's).
    Get {
        /// watch.enabled, watch.auto_pull, watch.auto_push, or watch.share_working
        key: String,

        /// Read ~/.oak/config.json only.
        #[arg(long)]
        global: bool,

        /// Emit machine-readable JSON (value, source, every layer).
        #[arg(long)]
        json: bool,
    },

    /// Set a key in this repo's config (or --global)
    ///
    /// Set a key in this checkout's repo config (or, with --global, in
    /// ~/.oak/config.json).
    Set {
        /// watch.enabled, watch.auto_pull, watch.auto_push, or watch.share_working
        key: String,

        /// true/false (also on/off, yes/no, 1/0)
        value: String,

        /// Write ~/.oak/config.json instead of the repo config.
        #[arg(long)]
        global: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Remove a key from the repo config (or --global)
    ///
    /// Remove a key from the repo config (or, with --global, the global file).
    Unset {
        /// watch.enabled, watch.auto_pull, watch.auto_push, or watch.share_working
        key: String,

        /// Edit ~/.oak/config.json instead of the repo config.
        #[arg(long)]
        global: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// List every key with its effective value and source.
    List {
        /// Show only ~/.oak/config.json.
        #[arg(long)]
        global: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
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
    },
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[derive(Subcommand)]
enum MountCommands {
    /// List mounts: live, stale, or orphaned
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Finish a mounted task: describe, commit, push, end
    Finish {
        /// Mount destination directory. Defaults to the current directory.
        dest: Option<PathBuf>,

        /// Read the final branch description from a UTF-8 file, or '-' for stdin.
        #[arg(long = "desc-file", value_name = "FILE")]
        desc_file: String,

        /// Emit one machine-readable JSON result on success.
        #[arg(long)]
        json: bool,
    },

    /// Unmount and remove a mount directory
    ///
    /// Unmount, drop local state, and remove the mount directory. With no DEST,
    /// tears down every mount under ~/oaktree.
    End {
        /// Mount destination directory. Omit to end every mount under ~/oaktree.
        dest: Option<PathBuf>,

        /// Discard any uncommitted overlay changes. Without this, `end`
        /// refuses to operate on a mount with dirty files so you don't
        /// silently lose work.
        #[arg(short, long)]
        force: bool,
    },

    /// Drop a stale mount registration
    ///
    /// Remove a mount's registry entry without unmounting or touching its
    /// on-disk state. For stale registrations left behind by a crash or reboot;
    /// refuses a live mount unless --force is given.
    ///
    /// `--orphaned` instead drops every non-live registration whose mount
    /// directory no longer exists (shown as `[orphaned]` by `oak mount
    /// list`); their on-disk state is kept and reported.
    Forget {
        /// Mount destination directory.
        #[arg(required_unless_present = "orphaned", conflicts_with = "orphaned")]
        dest: Option<PathBuf>,

        /// Drop the registration even if the mount looks live.
        #[arg(short, long, conflicts_with = "orphaned")]
        force: bool,

        /// Drop all registrations whose mount directory no longer exists.
        #[arg(long)]
        orphaned: bool,
    },

    /// Internal: run the blocking foreground server for one mount. Spawned
    /// detached by `oak mount`; not meant to be run by hand.
    #[command(name = "__serve", hide = true)]
    Serve {
        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,

        /// Repository spec in `org/repo` form (e.g. `oak/oak`)
        spec: String,

        /// Destination directory to mount onto. Created if it doesn't exist.
        dest: PathBuf,

        /// Branch to mount (defaults to the repo's default branch)
        #[arg(short, long)]
        branch: Option<String>,

        /// View to mount (canonical `ViewSpec`); absent for the full repo.
        #[arg(long)]
        view: Option<String>,
    },

    /// Internal: re-serve an existing mount from its intact state dir after
    /// the daemon died (crash, reboot). Spawned detached by the stale-mount
    /// recovery in `oak mount`; not meant to be run by hand.
    #[command(name = "__resume", hide = true)]
    Resume {
        /// Destination directory of the registered mount to resume.
        dest: PathBuf,
    },

    /// Claude Code `WorktreeCreate` hook: mount <spec> at the worktree path
    /// read from the hook's stdin JSON, then print the path. Wired into a
    /// project's `.claude/settings.json`; not meant to be run by hand.
    #[command(hide = true)]
    WorktreeCreate {
        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,

        /// Repository spec in `org/repo` form (e.g. `oak/oak`)
        spec: String,
    },

    /// Claude Code `WorktreeRemove` hook: unmount + clean up the mount at the
    /// worktree path read from the hook's stdin JSON. Wired into an Oak
    /// space's `.claude/settings.json`; not meant to be run by hand.
    #[command(hide = true)]
    WorktreeRemove,

    /// macOS FSKit XPC broker. Run on demand by the `com.oakvcs.mount` LaunchAgent;
    /// vends the mach service the sandboxed OakFS extension connects to. Not for
    /// manual use.
    #[command(name = "__fskit-broker", hide = true)]
    FskitBroker,

    /// Spike helper: ping the `com.oakvcs.mount` broker and print its echo. Not for
    /// manual use.
    #[command(name = "__fskit-broker-ping", hide = true)]
    FskitBrokerPing {
        /// Text to echo through the broker.
        #[arg(default_value = "hello-oak-xpc")]
        text: String,
    },
}

/// Flags shared by every `oak root` subcommand.
#[derive(clap::Args, Clone, Default)]
struct RootTargetArgs {
    /// Repository as OWNER/NAME (default: this checkout's repo, or the one
    /// the root directory you're in belongs to)
    #[arg(long, value_name = "OWNER/NAME")]
    repo: Option<String>,

    /// Remote server URL
    #[arg(short, long)]
    remote: Option<String>,

    /// Machine-readable output (list/show/create/archive print the server's
    /// JSON unchanged)
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum RootCommands {
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
    fn into_action(self) -> (RootTargetArgs, commands::root::RootAction, bool) {
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
struct SiteTargetArgs {
    /// Repository as OWNER/NAME (default: this checkout's repo)
    #[arg(long, value_name = "OWNER/NAME")]
    repo: Option<String>,

    /// Remote server URL
    #[arg(short, long)]
    remote: Option<String>,

    /// Print the server's JSON response
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum SiteCommands {
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
enum SiteDomainCommands {
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
    fn target(&self) -> &SiteTargetArgs {
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
    fn into_action(self) -> oak_core::Result<(SiteTargetArgs, commands::site::SiteAction, bool)> {
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
enum RetentionCommands {
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
enum SparseCommands {
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
enum GroupCommands {
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
    /// Put the org's agents (service accounts) on groups
    ///
    /// The org's agents (service accounts) on groups. An agent reaches what the
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
enum GroupAgentCommands {
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

#[derive(Subcommand)]
enum MaintenanceCommands {
    /// Compact the local database and reclaim space
    ///
    /// Compact the local database: convert to the compact tree/blob storage
    /// format, drop the legacy tree_entries table, and VACUUM to reclaim space.
    Compact,
}

/// Detect whether `cwd` lies inside a registered mount point.
///
/// Returns the canonical mount-point path if so, `None` otherwise. The
/// dispatch layer uses this to route commands like `oak commit/status/log`
/// to mount-aware code paths transparently — users see the same commands
/// they use in regular repos, but the operations target the virtual branch
/// and overlay state.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn mount_dest_for_cwd(cwd: &std::path::Path) -> Option<PathBuf> {
    commands::mount::mount_dest_for(cwd).ok().flatten()
}

/// On platforms without mount support, no path is ever a mount.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn mount_dest_for_cwd(_cwd: &std::path::Path) -> Option<PathBuf> {
    None
}

fn init_logging(always_enable: bool) {
    // For server: always enable logging
    // For CLI: only enable if OAK_LOG is set
    let should_enable = always_enable || std::env::var("OAK_LOG").is_ok();

    if should_enable {
        let filter = if let Ok(filter_str) = std::env::var("OAK_LOG") {
            tracing_subscriber::EnvFilter::new(filter_str)
        } else {
            tracing_subscriber::EnvFilter::new("info")
        };

        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

/// Whether this invocation asked for JSON: `--json` was given at any level
/// of the command path. Read from the parsed arguments, so it is decided
/// once, by clap, for every command — a command gains the JSON error
/// envelope by declaring a `--json` flag and nothing else. (The contract
/// tests assert every `--json` flag is a boolean with the argument id
/// `json`, which is what this reads.)
fn invocation_requests_json(matches: &clap::ArgMatches) -> bool {
    let mut level = Some(matches);
    while let Some(current) = level {
        if matches!(current.try_get_one::<bool>("json"), Ok(Some(true))) {
            return true;
        }
        level = current.subcommand().map(|(_, sub)| sub);
    }
    false
}

/// The same question when the arguments did not parse (so there are no
/// matches to read): a literal `--json` before any `--` separator. A usage
/// error under `--json` must still produce one JSON document on stdout.
fn argv_requests_json(args: &[String]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| match arg.strip_prefix("--json") {
            Some("") => true,
            // `--json=true` is not accepted by the flag, but it plainly
            // asks for JSON: the usage error it causes must be a document.
            Some(value) => value.starts_with('=') && value != "=false",
            None => false,
        })
}

/// Commands whose stdout is machine-readable rows or a hook protocol without
/// `--json` (`oak status --porcelain`, `oak watch hook`). Like JSON mode they
/// must stay free of progress animation and the daily version notice; unlike
/// JSON mode their failures are reported as text on stderr.
fn command_emits_machine_rows(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Status {
            porcelain: true,
            ..
        } | Commands::Status { short: true, .. }
            | Commands::Watch {
                action: Some(WatchCommands::Hook { .. }),
                ..
            }
    )
}

/// The single failure epilogue: every failing invocation leaves through
/// here, whether the arguments never parsed or the command ran and failed.
///
/// In JSON mode it writes exactly one JSON error document to stdout and
/// nothing about the error to stderr — unless a JSON document already
/// landed on stdout (`output::print_json` records that; a document that
/// reports a failing verdict embeds this error, see
/// `output::print_json_with_error`), in which case stdout is left alone and
/// the error goes to stderr. Otherwise the error goes to stderr. The exit
/// code, the code, and the advice all come from the one contract mapping
/// (`oak_cli::contract::classify_in`), given the context gathered here.
fn exit_with_error(
    e: &oak_core::OakError,
    json_mode: bool,
    cwd: Option<&std::path::Path>,
    remote_retry: Option<oak_cli::contract::RemoteRetry>,
) -> ! {
    // The `--remote` form of a branch command is only worth offering where
    // there is a remote to ask.
    let remote_retry = remote_retry.filter(|_| cwd.is_some_and(checkout_has_configured_remote));
    let context =
        oak_cli::contract::AdviceContext::for_process(cwd).with_remote_retry(remote_retry);
    let contract = oak_cli::contract::classify_in(e, &context);
    let code = contract.exit.code();
    // fb-83: when the mapping led with the `--remote` form (the branch asked
    // for is missing locally), say so in the message as well.
    let remote_form = context
        .remote_retry
        .as_ref()
        .filter(|retry| contract.next.first() == Some(&retry.command))
        .map(|retry| retry.command.render());
    if json_mode && !output::json_payload_emitted() {
        let mut envelope = output::JsonErrorEnvelope::from_error_in(e, &context, cwd);
        if let Some(command) = &remote_form {
            envelope.error.message.push_str(&format!(
                " (not found locally; it may exist only on the remote: `{command}`)"
            ));
        }
        if let Err(print_err) = output::print_json_error_envelope(&envelope) {
            output::error(&print_err.to_string());
        }
        // The envelope IS the error report, and it goes to *stdout*. If
        // stdout was genuinely unwritable the caller received nothing at
        // all, so leave through the checked exit: it reports the loss on
        // stderr (where a JSON consumer can still see it) and keeps the
        // error's own nonzero code. A vanished pipe reader records nothing
        // and is unaffected.
        output::exit_process(code);
    }
    // The non-JSON error report itself is stderr-only, so it cannot be lost
    // here. What makes skipping the stdout-health check safe is the exit
    // code, not an absence of earlier stdout writes: the contract's exit
    // table has no 0, so this path cannot claim success and an unreported
    // stdout loss can never be mistaken for a clean run. (Plenty of commands
    // do write stdout before failing into this branch — `oak reset` prints
    // the whole discard list, then can fail materializing it. The report the
    // user is entitled to is the error, and that is on stderr.) Re-reporting
    // a recorded stdout error here would also print it twice whenever `e`
    // *is* that error, arriving from the epilogue chain.
    match &remote_form {
        Some(command) => output::error(&format!(
            "{e} (not found locally; it may exist only on the remote). Try: {command}"
        )),
        None => output::error(&output::bounded_error_message(e)),
    }
    std::process::exit(code);
}

/// A clap usage error, through the same epilogue as every other failure.
/// JSON mode: one `invalid_argument` document on stdout, exit 2. Otherwise
/// clap's own rendering on stderr, exit 2.
fn exit_with_usage_error(err: clap::Error, json_mode: bool) -> ! {
    if !json_mode {
        let _ = err.print();
        std::process::exit(oak_core::ExitCode::Usage.code());
    }
    let rendered = err.render().to_string();
    let message = rendered.trim();
    let message = message.strip_prefix("error: ").unwrap_or(message);
    exit_with_error(
        &oak_core::OakError::Usage(message.to_string()),
        true,
        None,
        None,
    )
}

/// Whether `line` is an `oak ...` command line the real argument parser
/// accepts. Installed as the debug-build check on every recommended command
/// a JSON document carries (`output::install_command_validator`), and used
/// by the contract tests.
fn recommended_command_parses(line: &str) -> Result<(), String> {
    let words = oak_cli::contract::split_command_line(line)
        .ok_or_else(|| format!("`{line}` is not a well-formed command line"))?;
    if words.first().map(String::as_str) != Some("oak") {
        return Err(format!("`{line}` does not start with `oak`"));
    }
    match Cli::try_parse_from(&words) {
        Ok(_) => Ok(()),
        // `oak <command> --help` is a valid invocation that clap reports as
        // an "error" carrying the help text.
        Err(err) if err.kind() == clap::error::ErrorKind::DisplayHelp => Ok(()),
        Err(err) => Err(format!("`{line}` does not parse: {}", err.render())),
    }
}

/// A small JSON receipt for a command whose human output is narration: the
/// command ran with its narration routed to stderr, succeeded, and this is
/// its one document. `facts` are values observed after it finished.
fn print_receipt(kind: &str, facts: serde_json::Value) -> oak_core::Result<()> {
    let mut receipt = serde_json::Map::new();
    receipt.insert("schema_version".to_string(), serde_json::json!(1));
    receipt.insert("kind".to_string(), serde_json::json!(kind));
    if let serde_json::Value::Object(facts) = facts {
        receipt.extend(facts);
    }
    output::print_json(&serde_json::Value::Object(receipt))
}

/// How many paths a reset/restore receipt lists before it truncates.
const DISCARD_RECEIPT_PATH_LIMIT: usize = 200;

/// The receipt body of `oak reset|restore --json`: what the command itself
/// reverted and deleted (nothing, when there was nothing to do), not the
/// arguments it was given.
fn discard_receipt(outcome: &commands::restore::DiscardOutcome) -> serde_json::Value {
    let bounded = |paths: &[String]| -> Vec<String> {
        paths
            .iter()
            .take(DISCARD_RECEIPT_PATH_LIMIT)
            .cloned()
            .collect()
    };
    serde_json::json!({
        "changed": !(outcome.reverted.is_empty() && outcome.deleted.is_empty()),
        "reverted_count": outcome.reverted.len(),
        "deleted_count": outcome.deleted.len(),
        "reverted": bounded(&outcome.reverted),
        "deleted": bounded(&outcome.deleted),
        "paths_truncated": outcome.reverted.len() > DISCARD_RECEIPT_PATH_LIMIT
            || outcome.deleted.len() > DISCARD_RECEIPT_PATH_LIMIT,
        "recommended_next_commands": [
            oak_cli::contract::NextCommand::StatusJsonCompact.render()
        ],
    })
}

/// The checkout's current branch name and head, read back from the
/// repository (read-only). `None` fields mean "not known", never a guess.
fn observed_branch_and_head(cwd: &std::path::Path) -> (Option<String>, Option<String>) {
    use oak_core::Repository;
    let Some(repo) = open_repo_read_only(cwd) else {
        return (None, None);
    };
    let branch = repo.get_current_branch_name().ok().flatten();
    // The checked-out branch's own head, as `oak status` reports it. The
    // repository-wide head record is only the answer for a detached
    // checkout: it can name another branch's commit (a parent sync
    // completed for a branch that is no longer checked out).
    let head = match &branch {
        Some(name) => repo.get_branch_head(name).ok().flatten(),
        None => repo.get_head().ok().flatten(),
    }
    .map(|hash| hash.to_string());
    (branch, head)
}

/// The local head of `branch`, read back from the repository (read-only).
fn observed_branch_head(cwd: &std::path::Path, branch: &str) -> Option<String> {
    use oak_core::Repository;
    open_repo_read_only(cwd)?
        .get_branch_head(branch)
        .ok()
        .flatten()
        .map(|hash| hash.to_string())
}

/// Whether a literal `--` appeared on the command line, and how many
/// arguments followed it. Clap folds them into the positional list without
/// recording the separator, but `oak diff` needs both facts: positionals
/// after `--` are always paths, and the presence of `--` itself is the
/// user's disambiguation that everything before it is a revision.
fn positionals_after_double_dash() -> Option<usize> {
    let mut seen = false;
    let mut count = 0usize;
    for arg in std::env::args_os().skip(1) {
        if seen {
            count += 1;
        } else if arg == "--" {
            seen = true;
        }
    }
    seen.then_some(count)
}

/// Usage-error text for `oak commit -m`/message-like positionals (fb-98).
/// Leads with the inline `oak desc "..."` form and says when the current
/// branch already carries a description (so `oak desc` would replace it).
/// The description probe is best-effort and strictly read-only.
fn commit_message_refusal(cwd: &std::path::Path, lead: &str, tail: &str) -> String {
    let existing = if current_branch_has_description(cwd) {
        " This branch already has a description; `oak desc` replaces it."
    } else {
        ""
    };
    format!(
        "{lead} Oak commits are messageless; write the branch narrative with `oak desc \"<summary>\"` (or `oak desc --file <file>`) before finishing.{existing}{tail}"
    )
}

/// Best-effort, strictly read-only open of the native repo at `cwd`, for
/// diagnostics that must never create or migrate `.oak/oak.db`.
fn open_repo_read_only(cwd: &std::path::Path) -> Option<oak_core::SqliteRepository> {
    let ctx = oak_cli::resolve::resolve(cwd).ok()?;
    let db_path = ctx.db_path().ok()?;
    oak_core::SqliteRepository::open_read_only(&db_path).ok()
}

fn current_branch_has_description(cwd: &std::path::Path) -> bool {
    use oak_core::Repository;
    let Some(repo) = open_repo_read_only(cwd) else {
        return false;
    };
    repo.get_current_branch_name()
        .ok()
        .flatten()
        .and_then(|name| repo.get_branch(&name).ok().flatten())
        .and_then(|branch| branch.description)
        .is_some_and(|description| !description.trim().is_empty())
}

/// For a local (non-`--remote`) `oak branch show|diff|review NAME`, the exact
/// `--remote` invocation to recommend if NAME turns out not to exist locally
/// (fb-83), as a typed command.
fn branch_remote_retry_command(command: &Commands) -> Option<oak_cli::contract::RemoteRetry> {
    use oak_cli::contract::{NextCommand, RemoteRetry};
    let Commands::Branch {
        action: Some(action),
        ..
    } = command
    else {
        return None;
    };
    let (branch, command) = match action {
        BranchCommands::Show {
            name,
            remote: false,
            ..
        } => (name, NextCommand::BranchShowRemote { name: name.clone() }),
        BranchCommands::Diff {
            name,
            against,
            diff_mode,
            paths,
            remote: false,
            ..
        } => (
            name,
            NextCommand::BranchDiffRemote {
                name: name.clone(),
                against: (against != "main").then(|| against.clone()),
                diff_mode: (diff_mode != "tree").then(|| diff_mode.clone()),
                paths: paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect(),
            },
        ),
        BranchCommands::Review {
            name,
            merge_preview,
            remote: false,
            ..
        } => (
            name,
            NextCommand::BranchReviewRemote {
                name: name.clone(),
                merge_preview: *merge_preview,
            },
        ),
        _ => return None,
    };
    Some(RemoteRetry {
        branch: branch.clone(),
        command,
    })
}

/// Whether this checkout has a remote to retry against: `OAK_REMOTE` or a
/// stored remote URL (read-only probe).
fn checkout_has_configured_remote(cwd: &std::path::Path) -> bool {
    use oak_core::Repository;
    if commands::push::env_remote_override().is_some() {
        return true;
    }
    open_repo_read_only(cwd)
        .and_then(|repo| {
            repo.get_metadata(oak_core::MetadataKey::RemoteUrl)
                .ok()
                .flatten()
        })
        .is_some_and(|url| !url.trim().is_empty())
}

fn commit_positional_looks_like_message(cwd: &std::path::Path, paths: &[PathBuf]) -> bool {
    if positionals_after_double_dash().is_some() || paths.len() != 1 {
        return false;
    }
    let arg = paths[0].as_os_str().to_string_lossy();
    !cwd.join(&paths[0]).exists() && arg.chars().any(char::is_whitespace)
}

/// The exact command that re-sends the same description, or `None` when it
/// can't be replayed verbatim (stdin, or inline text too long to quote into a
/// command line). Never a placeholder: agents may run it literally. User text
/// always follows `--` (or is attached with `=`), so text that looks like a
/// flag (`--file=/etc/hosts`, `--help`) is replayed as the description, not
/// parsed as options.
fn desc_retry_command(
    inline: Option<&str>,
    file: Option<&str>,
    cwd: &std::path::Path,
) -> Option<String> {
    const MAX_INLINE_RETRY_BYTES: usize = 2048;
    if let Some(text) = inline {
        return (text.len() <= MAX_INLINE_RETRY_BYTES)
            .then(|| format!("oak desc --json -- {}", output::shell_quote(text)));
    }
    let file = file.filter(|file| *file != "-")?;
    let path = cwd.join(file);
    Some(format!(
        "oak desc --json --file={}",
        output::shell_quote(&commands::absolute_path_string(&path))
    ))
}

/// Inline descriptions are stored verbatim. Shells do not expand `\n` inside
/// ordinary quotes, so `oak desc "line one\nline two"` stores a literal
/// backslash-n that later lands in the squash-merge message (fb-177). Warn
/// (never rewrite) when the text has escape sequences but no real newline.
fn warn_literal_newline_escapes(command: &str, description: &str) {
    if description.contains("\\n") && !description.contains('\n') {
        output::warning(&format!(
            "{command} description contains literal `\\n` sequences and no real newlines; it is stored verbatim. \
             For multi-line text use `{command} --{flag} <file>` or $'...\\n...' shell quoting.",
            flag = if command == "oak finish" { "desc-file" } else { "file" },
        ));
    }
}

/// `--wait` takes an optional value, so `oak merge --wait my-branch` reads
/// the branch name as the timeout (fb-363). Keep `--wait 15` / `--wait=15`
/// working, but turn a non-numeric value into a usage error that names the
/// intended invocation instead of a bare "invalid digit".
fn parse_merge_wait_minutes(value: &str) -> Result<u64, String> {
    value.parse::<u64>().map_err(|_| {
        if value.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '+') {
            format!("`{value}` is not a whole number of minutes")
        } else {
            format!(
                "`--wait` takes an optional timeout in minutes, so `{value}` was read as the timeout. \
                 To merge branch `{value}`, put the branch first: `oak merge {value} --wait` \
                 (or `oak merge {value} --wait=N` for an N-minute timeout)"
            )
        }
    })
}

fn read_desc_input(
    command: &str,
    description: Option<String>,
    file: Option<String>,
) -> oak_core::Result<String> {
    match (description, file) {
        (Some(description), None) => {
            warn_literal_newline_escapes(command, &description);
            Ok(description)
        }
        (None, Some(file)) if file == "-" => {
            let mut description = String::new();
            std::io::stdin().read_to_string(&mut description)?;
            Ok(description)
        }
        (None, Some(file)) => std::fs::read_to_string(file).map_err(oak_core::OakError::Io),
        (None, None) => Err(oak_core::OakError::InvalidArgument(format!(
            "{command} requires a description or description file"
        ))),
        (Some(_), Some(_)) => Err(oak_core::OakError::InvalidArgument(format!(
            "{command} accepts either a description or description file, not both"
        ))),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Bare `oak`, `oak --help`/`-h`, and `oak help` all show the curated
    // overview below; `oak help <command>` falls through to clap's
    // per-command help.
    if args.len() <= 1 || (args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h" | "help"))
    {
        print_help();
        // Help exits without reaching the result epilogue below, so it must
        // do its own stdout-health check: a genuine write failure means the
        // help never arrived and cannot exit 0.
        output::exit_process(0);
    }

    // Intercept clap's own stdout rendering (subcommand `--help`/`help`,
    // `--version`): `Cli::parse()` would print directly to stdout and exit,
    // bypassing both the broken-pipe-safe output funnel and the stdout-health
    // check — a vanished pipe reader would panic inside clap and a genuine
    // write failure would be silently dropped. Render through the funnel and
    // exit through the checked path instead; the ANSI-vs-plain choice comes
    // from the repository's one color policy (see `output::clap_message_text`).
    // Usage errors keep clap's stderr rendering and exit code 2 (or, under
    // `--json`, become one `invalid_argument` document on stdout).
    // Usage errors leave through the failure epilogue like any other error.
    use clap::{CommandFactory, FromArgMatches};
    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        Err(err) => match err.kind() {
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                output::print_raw(&output::clap_message_text(
                    &err.render(),
                    output::stdout_colors_enabled(),
                ));
                output::exit_process(0);
            }
            _ => exit_with_usage_error(err, argv_requests_json(&args)),
        },
    };
    // Debug builds (what the test suites run) check every recommended
    // command of every JSON document against this parser.
    output::install_command_validator(recommended_command_parses);
    // JSON mode is a property of the invocation, decided here, once.
    let json_mode = invocation_requests_json(&matches);
    let mut cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => exit_with_usage_error(err.format(&mut Cli::command()), json_mode),
    };
    cli.command = cli.command.resolve_aliases();

    // reqwest uses rustls with no built-in crypto provider (we keep aws-lc-sys
    // out of the build — see Cargo.toml). Install `ring` as the process-default
    // CryptoProvider before any TLS happens. Every client constructor also does
    // this, so library/test paths are covered too.
    oak_cli::http::ensure_crypto_provider();

    // CLI commands only log when OAK_LOG is set; there's no long-running
    // process here that wants always-on logging.
    init_logging(false);

    // Enable verbose timing output if --verbose or OAK_VERBOSE is set
    if cli.verbose || std::env::var("OAK_VERBOSE").is_ok() {
        output::enable_verbose();
        output::vlog("verbose mode enabled");
    }

    // The shell's directory can vanish underneath it (deleted, or replaced by
    // a fresh clone/mount at the same path). That is a user-environment
    // problem, not a bug, so report it plainly instead of panicking.
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            let detail = if e.kind() == std::io::ErrorKind::NotFound {
                "the current directory no longer exists (it may have been deleted or replaced). \
                 Run `cd` to a valid directory and try again."
                    .to_string()
            } else {
                format!("cannot read the current directory: {e}")
            };
            exit_with_error(
                &oak_core::OakError::Io(std::io::Error::new(e.kind(), detail)),
                json_mode,
                None,
                None,
            );
        }
    };

    // Create a single tokio runtime for all async commands
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // `oak upgrade` already does its own version check; skip the daily notice
    // for it to avoid a redundant prompt.
    let skip_version_notice = matches!(
        cli.command,
        Commands::Upgrade { .. }
            | Commands::Completions { .. }
            | Commands::Mcp { .. }
            | Commands::File { .. }
            | Commands::Refs { .. }
            | Commands::Tree { .. }
            | Commands::Environment { .. }
            | Commands::Change { .. }
            | Commands::Watch { .. }
            | Commands::Branch {
                action: Some(BranchCommands::Train { .. }),
                ..
            }
    );
    let structured_output = json_mode || command_emits_machine_rows(&cli.command);
    let skip_version_notice = skip_version_notice || structured_output;
    let branch_remote_retry = branch_remote_retry_command(&cli.command);
    output::configure_activity(structured_output);
    // The `WorktreeCreate` hook must emit nothing on stdout but the worktree
    // path, and `WorktreeRemove` runs at session teardown — a stray "new
    // version available" line would corrupt the hook output, so suppress the
    // daily notice for both.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    let skip_version_notice = skip_version_notice
        || matches!(
            cli.command,
            Commands::Mount {
                action: Some(MountCommands::WorktreeCreate { .. } | MountCommands::WorktreeRemove),
                ..
            }
        );

    // A view checkout sends its view on every request, so the
    // server answers in the view commits it holds. Commands that create a
    // checkout start from no view: cloning the same repo in full from inside
    // a view checkout must not inherit it.
    let creates_checkout = matches!(cli.command, Commands::Clone(_) | Commands::Init { .. });
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    let creates_checkout = creates_checkout || matches!(cli.command, Commands::Mount { .. });
    if !creates_checkout {
        if let Some(view) = checkout_view(&cwd) {
            oak_cli::http::set_active_view(Some(view));
        }
    }

    let result = match cli.command {
        Commands::Init { path, git, json } => {
            let target = path
                .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
                .unwrap_or_else(|| cwd.clone());
            if git {
                commands::git_remote::init(&target)
            } else if json {
                // Never interactive under --json: no prompts, narration on
                // stderr, one receipt of what was created.
                output::route_narration_to_stderr();
                commands::init::run_reporting(&target, false).and_then(|outcome| {
                    print_receipt(
                        "init",
                        serde_json::json!({
                            "path": commands::absolute_path_string(&target),
                            "branch": outcome.branch,
                            "imported_commit_count": outcome.imported_commits,
                            "recommended_next_commands": [
                                oak_cli::contract::NextCommand::StatusJsonCompact.render()
                            ],
                        }),
                    )
                })
            } else {
                commands::init::run(&target, std::io::IsTerminal::is_terminal(&std::io::stdin()))
            }
        }

        Commands::Commit {
            paths,
            json,
            message,
            no_verify,
            push,
            quiet,
        } => {
            if message.is_some() {
                Err(oak_core::OakError::InvalidArgument(commit_message_refusal(
                    &cwd,
                    "oak commit does not take a message.",
                    "",
                )))
            } else if commit_positional_looks_like_message(&cwd, &paths) {
                Err(oak_core::OakError::InvalidArgument(commit_message_refusal(
                    &cwd,
                    "oak commit takes path filters, not a message.",
                    " Use `oak commit -- <path>` for a deleted path that no longer exists.",
                )))
            } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Mounted commits don't go through the local working tree, so
                // local hooks (which would run against the wrong files) are
                // intentionally not invoked here.
                rt.block_on(commands::mount::commit_with_options(
                    &dest, &cwd, &paths, json, push, quiet,
                ))
            } else {
                commands::commit::run_with_options(
                    &cwd,
                    commands::commit::CommitOptions {
                        no_verify,
                        paths,
                        push,
                        json,
                        quiet,
                    },
                )
            }
        }

        Commands::Merge {
            r#continue,
            abort,
            dry_run,
            offline,
            branch,
            into_parent,
            json,
            force,
            wait,
        } => {
            if dry_run {
                if !json {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak merge --dry-run` currently requires --json".to_string(),
                    ))
                } else if r#continue || abort {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak merge --dry-run` cannot be combined with --continue or --abort"
                            .to_string(),
                    ))
                } else if wait.is_some() {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak merge --wait` cannot be combined with --dry-run".to_string(),
                    ))
                } else if force {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak merge --force` cannot be combined with --dry-run".to_string(),
                    ))
                } else {
                    commands::review::merge_preview_branch_json(
                        &cwd,
                        branch.as_deref(),
                        offline,
                        &rt,
                    )
                }
            } else if wait.is_some() && (r#continue || abort) {
                Err(oak_core::OakError::InvalidArgument(
                    "`oak merge --wait` cannot be combined with --continue or --abort".to_string(),
                ))
            } else if force && (r#continue || abort) {
                Err(oak_core::OakError::InvalidArgument(
                    "`oak merge --force` cannot be combined with --continue or --abort".to_string(),
                ))
            } else {
                rt.block_on(commands::merge::run_with_stack_policy(
                    &cwd,
                    r#continue,
                    abort,
                    branch.as_deref(),
                    force,
                    json,
                    if into_parent {
                        commands::merge::StackedMerge::FoldIntoParent
                    } else {
                        commands::merge::StackedMerge::Refuse
                    },
                ))
            }
        }

        Commands::Switch {
            name,
            create,
            stack,
            parent,
            clean,
            discard,
            detach,
            // fb-116: `--remote` is an explicit spelling of the default
            // resolution (local branch, else fetched from the remote).
            remote: _,
            json,
        } => {
            let before = json.then(|| {
                output::route_narration_to_stderr();
                observed_branch_and_head(&cwd)
            });
            let switched = if json && name.is_none() && !create {
                Err(oak_core::OakError::InvalidArgument(
                    "`oak switch --json` needs a branch name (or -c): the interactive picker cannot run under --json"
                        .to_string(),
                ))
            } else
            // A mount is pinned to its virtual branch for its whole life —
            // there is no working tree to re-materialize, so `oak switch`
            // can't work here. Without this check the resolver walks past the
            // mount to some unrelated repo and fails with a misleading error
            // (e.g. "uncommitted changes" against a clean mount).
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                Err(oak_core::OakError::InvalidArgument(
                    commands::mount::switch_unsupported_message(&dest),
                ))
            } else if create {
                if detach {
                    Err(oak_core::OakError::Io(std::io::Error::other(
                        "switch -c cannot be combined with --detach",
                    )))
                } else {
                    let policy =
                        commands::switch::WorktreePolicy::from_clean_flag(clean || discard);
                    match commands::switch::StackParent::from_flags(stack, parent) {
                        Some(stack_parent) => commands::switch::create_stacked(
                            &cwd,
                            name.as_deref(),
                            stack_parent,
                            policy,
                        ),
                        None => match name {
                            Some(name) => commands::switch::create(&cwd, &name, policy),
                            None => commands::switch::fresh(&cwd, policy),
                        }
                        .map(|_created| ()),
                    }
                }
            } else if discard {
                Err(oak_core::OakError::Io(std::io::Error::other(
                    "switch --discard requires -c; use `oak switch NAME --clean` to require a clean switch to an existing branch",
                )))
            } else {
                let policy = commands::switch::WorktreePolicy::from_clean_flag(clean);
                commands::switch::run_with_policy(&cwd, name.as_deref(), detach, policy)
            };
            match (switched, before) {
                (Ok(()), Some((previous_branch, previous_head))) => {
                    let (branch, head) = observed_branch_and_head(&cwd);
                    print_receipt(
                        "switch",
                        serde_json::json!({
                            "branch": branch,
                            "head": head,
                            "previous_branch": previous_branch,
                            "previous_head": previous_head,
                            "recommended_next_commands": [
                                oak_cli::contract::NextCommand::StatusJsonCompact.render()
                            ],
                        }),
                    )
                }
                (outcome, _) => outcome,
            }
        }
        Commands::Close {
            names,
            remote,
            json,
            reason,
        } if names.len() > 1
            || names.iter().any(|arg| {
                commands::branch_close::looks_pinned(arg, |literal| {
                    commands::branch_close::local_branch_exists(&cwd, literal)
                })
            }) =>
        {
            // fb-413: exact-head fenced, per-branch atomic close of one or
            // more remote branches.
            if !(remote && json) {
                Err(oak_core::OakError::InvalidArgument(
                    "closing several branches or BRANCH@HEAD targets requires --remote --json"
                        .to_string(),
                ))
            } else {
                match commands::branch_close::parse_targets(&names).and_then(|targets| {
                    let close_reason = reason
                        .as_deref()
                        .map(oak_core::CloseReason::parse)
                        .transpose()?;
                    Ok((targets, close_reason))
                }) {
                    Err(e) => Err(e),
                    Ok((targets, close_reason)) => {
                        match rt.block_on(commands::branch_close::close_many_remote_json(
                            &cwd,
                            &targets,
                            close_reason,
                        )) {
                            Ok(code) => {
                                output::set_exit_status(code);
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
            }
        }
        Commands::Close {
            names,
            remote,
            json,
            reason,
        } => {
            // No name: the current branch of the checkout here. Not being in
            // a repository at all is its own failure (`repo_not_found`).
            let resolved: oak_core::Result<String> = match names.into_iter().next() {
                Some(name) => Ok(name),
                None => oak_cli::resolve::resolve(&cwd)
                    .and_then(|ctx| ctx.open())
                    .and_then(|repo| repo.get_current_branch_name())
                    .and_then(|current| {
                        current.ok_or_else(|| {
                            oak_core::OakError::BranchNotFound(
                                "no branch name provided and no current branch set".to_string(),
                            )
                        })
                    }),
            };
            let (name, close_reason) = match resolved {
                Err(error) => (String::new(), Err(error)),
                Ok(name) => (
                    name,
                    match reason {
                        None => Ok(None),
                        Some(value) => oak_core::CloseReason::parse(&value).map(Some),
                    },
                ),
            };
            match close_reason {
                Err(e) => Err(e),
                Ok(close_reason) => {
                    if remote {
                        if json {
                            rt.block_on(commands::branch::close_remote_branch_json(
                                &cwd,
                                &name,
                                close_reason,
                            ))
                        } else {
                            Err(oak_core::OakError::InvalidArgument(
                                "`oak close --remote` currently requires --json".to_string(),
                            ))
                        }
                    } else if json {
                        rt.block_on(commands::branch::close_branch_json(
                            &cwd,
                            &name,
                            close_reason,
                        ))
                    } else {
                        rt.block_on(commands::branch::close_branch(&cwd, &name, close_reason))
                    }
                }
            }
        }
        Commands::Desc {
            description,
            file,
            json,
            append,
        } => {
            let retry_command = desc_retry_command(description.as_deref(), file.as_deref(), &cwd);
            read_desc_input("oak desc", description, file).and_then(|description| {
                if append {
                    if json {
                        rt.block_on(commands::branch::append_current_branch_json(
                            &cwd,
                            &description,
                        ))
                        .and_then(|receipt| output::print_json(&receipt))
                    } else {
                        rt.block_on(commands::branch::append_current_branch(&cwd, &description))
                    }
                } else if json {
                    let receipt = if let Some(dest) = mount_dest_for_cwd(&cwd) {
                        rt.block_on(commands::mount::desc_json(
                            &dest,
                            &description,
                            retry_command,
                        ))
                    } else {
                        rt.block_on(commands::branch::edit_current_branch_json(
                            &cwd,
                            &description,
                            retry_command,
                        ))
                    };
                    receipt.and_then(|receipt| output::print_json(&receipt))
                } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                    // Inside a mount, set the description on the virtual branch
                    // in the mount cache db (and best-effort-push it to the
                    // server). The regular `edit_current_branch` path can't see
                    // the virtual branch because it opens the local-repo SQLite
                    // rather than the mount cache.
                    rt.block_on(commands::mount::desc(&dest, &description))
                } else {
                    rt.block_on(commands::branch::edit_current_branch(&cwd, &description))
                }
            })
        }
        Commands::Finish {
            desc,
            desc_file,
            json,
        } => read_desc_input("oak finish", desc, desc_file).and_then(|description| {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                if json {
                    rt.block_on(commands::mount::finish_json(&dest, &description))
                        .and_then(|result| output::print_json(&result))
                } else {
                    rt.block_on(commands::mount::finish(&dest, &description))
                }
            } else {
                // The task is ending: stop `oak watch` first so its
                // auto-push cannot race finish's publication, and mark the
                // branch idle.
                let watch_was_running = rt.block_on(commands::watch::stop_for_finish(&cwd));
                let result = if json {
                    rt.block_on(commands::finish::run_json(&cwd, &description))
                        .and_then(|result| output::print_json(&result))
                } else {
                    rt.block_on(commands::finish::run(&cwd, &description))
                };
                // A preflight refusal changed nothing and the task goes on.
                if watch_was_running
                    && matches!(result, Err(oak_core::OakError::FinishPreflight(_)))
                {
                    commands::watch::resume_after_refused_finish(&cwd);
                }
                result
            }
        }),
        Commands::Branch {
            json,
            show_current,
            action,
        } => {
            if show_current {
                if json || action.is_some() {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak branch --show-current` cannot be combined with --json or branch subcommands".to_string(),
                    ))
                } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                    commands::mount::show_current_branch(&dest)
                } else {
                    commands::branch::show_current_branch(&cwd)
                }
            } else {
                match action {
                    None => {
                        if json {
                            commands::branch::list_branches_json(&cwd, None)
                        } else {
                            commands::branch::list_branches(&cwd)
                        }
                    }
                    Some(BranchCommands::List {
                        json: list_json,
                        remote,
                        status,
                    }) => {
                        if remote {
                            if json || list_json {
                                rt.block_on(commands::branch::list_remote_branches_json(
                                    &cwd,
                                    status.as_deref(),
                                ))
                            } else {
                                Err(oak_core::OakError::InvalidArgument(
                                    "`oak branch list --remote` currently requires --json"
                                        .to_string(),
                                ))
                            }
                        } else if json || list_json {
                            commands::branch::list_branches_json(&cwd, status.as_deref())
                        } else {
                            commands::branch::list_branches(&cwd)
                        }
                    }
                    Some(BranchCommands::Show {
                        name,
                        json: show_json,
                        remote,
                    }) => {
                        if remote {
                            if json || show_json {
                                rt.block_on(commands::branch::show_remote_branch_json(&cwd, &name))
                            } else {
                                Err(oak_core::OakError::InvalidArgument(
                                    "`oak branch show --remote` currently requires --json"
                                        .to_string(),
                                ))
                            }
                        } else if json || show_json {
                            commands::branch::show_branch_json(&cwd, &name)
                        } else {
                            commands::branch::show_branch(&cwd, &name)
                        }
                    }
                    Some(BranchCommands::Diff {
                        name,
                        against,
                        diff_mode,
                        changed_files_limit,
                        changed_files_offset,
                        json: diff_json,
                        hunks,
                        max_bytes,
                        unified,
                        text,
                        remote,
                        print,
                        line_numbers,
                        repo,
                        paths,
                    }) => match commands::review::DiffMode::parse(&diff_mode) {
                        Err(err) => Err(err),
                        Ok(_) if !(hunks || print)
                            && (max_bytes.is_some() || unified.is_some() || text || line_numbers) =>
                        {
                            Err(oak_core::OakError::InvalidArgument(
                                "--max-bytes, -U/--unified, -a/--text and --line-numbers require --hunks (or --remote --print)"
                                    .to_string(),
                            ))
                        }
                        Ok(diff_mode) => {
                            let options = commands::review::DiffJsonOptions {
                                changed_files_limit,
                                changed_files_offset,
                                hunks,
                                max_bytes,
                                context: unified.unwrap_or(oak_core::DEFAULT_CONTEXT_LINES),
                                force_text: text,
                                line_numbers,
                            };
                            if remote {
                                if json || diff_json {
                                    rt.block_on(
                                        commands::review::server_review::branch_diff_json(
                                            &cwd,
                                            repo.as_deref(),
                                            &name,
                                            &against,
                                            diff_mode,
                                            &paths,
                                            options,
                                        ),
                                    )
                                } else if print {
                                    rt.block_on(
                                        commands::review::server_review::branch_diff_print(
                                            &cwd,
                                            repo.as_deref(),
                                            &name,
                                            &against,
                                            diff_mode,
                                            &paths,
                                            options,
                                        ),
                                    )
                                } else {
                                    Err(oak_core::OakError::InvalidArgument(
                                        "`oak branch diff --remote` requires --json or --print"
                                            .to_string(),
                                    ))
                                }
                            } else if json || diff_json {
                                commands::review::branch_diff_json(
                                    &cwd, &name, &against, diff_mode, &paths, options,
                                )
                            } else {
                                // Human-readable branch diff: same endpoint
                                // presentation `oak diff <branch>` uses —
                                // stat table on a pipe, browser on a TTY.
                                commands::diff::branch_diff_human(
                                    &cwd, &name, &against, diff_mode, &paths,
                                )
                                .map(|_| ())
                            }
                        }
                    },
                    Some(BranchCommands::Review {
                        name,
                        merge_preview,
                        changed_files_limit,
                        changed_files_offset,
                        json: review_json,
                        remote,
                        repo,
                    }) => {
                        if remote {
                            if json || review_json {
                                rt.block_on(commands::review::server_review::branch_review_json(
                                    &cwd,
                                    repo.as_deref(),
                                    &name,
                                    merge_preview,
                                    "main",
                                    changed_files_limit,
                                    changed_files_offset,
                                ))
                            } else {
                                Err(oak_core::OakError::InvalidArgument(
                                    "`oak branch review --remote` currently requires --json"
                                        .to_string(),
                                ))
                            }
                        } else if json || review_json {
                            commands::review::branch_review_json(
                                &cwd,
                                &name,
                                merge_preview,
                                "main",
                                changed_files_limit,
                                changed_files_offset,
                                Some(&rt),
                            )
                        } else {
                            Err(oak_core::OakError::InvalidArgument(
                                "`oak branch review` currently requires --json".to_string(),
                            ))
                        }
                    }
                    Some(BranchCommands::Train {
                        names,
                        against,
                        remote,
                        json: train_json,
                    }) => rt
                        .block_on(commands::review::train::run(
                            &cwd,
                            &names,
                            &against,
                            remote,
                            json || train_json,
                        ))
                        .map(|complete| {
                            if !complete {
                                output::set_exit_status(6);
                            }
                        }),
                    Some(BranchCommands::Rename { old_name, new_name }) => {
                        if json {
                            Err(oak_core::OakError::InvalidArgument(
                                "`oak branch rename` does not support --json".to_string(),
                            ))
                        } else {
                            commands::branch::rename_branch(&cwd, &old_name, &new_name)
                        }
                    }
                    Some(BranchCommands::Close { .. }) => {
                        unreachable!("`oak branch close` is rewritten by Commands::resolve_aliases")
                    }
                    Some(BranchCommands::Triage {
                        remote,
                        against,
                        status,
                        analysis_depth,
                        only,
                        limit,
                        json: triage_json,
                    }) => {
                        if !(json || triage_json) {
                            Err(oak_core::OakError::InvalidArgument(
                                "`oak branch triage` currently requires --json".to_string(),
                            ))
                        } else {
                            commands::triage::run_branch_triage_command(
                                &cwd,
                                &rt,
                                remote,
                                &against,
                                status.as_deref(),
                                &analysis_depth,
                                only.as_deref(),
                                limit,
                            )
                        }
                    }
                }
            }
        }
        Commands::Agent { action } => match action {
            AgentCommands::State {
                json,
                refresh,
                compact,
                publication_after,
            } => {
                if !json {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak agent state` currently requires --json".to_string(),
                    ))
                } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                    rt.block_on(commands::mount::agent_state_json_paged(
                        &dest,
                        refresh,
                        compact,
                        publication_after.as_deref(),
                    ))
                } else {
                    rt.block_on(commands::status::run_agent_state_json(
                        &cwd,
                        refresh,
                        compact,
                        publication_after.as_deref(),
                    ))
                }
            }
            AgentCommands::Publication { action } => match action {
                AgentPublicationCommands::Forget {
                    operation_id,
                    acknowledge_unknown,
                    json,
                } => {
                    if !acknowledge_unknown || !json {
                        Err(oak_core::OakError::InvalidArgument(
                            "`oak agent publication forget` requires --acknowledge-unknown --json"
                                .to_string(),
                        ))
                    } else {
                        (|| -> oak_core::Result<()> {
                            let store = oak_cli::publication_state::store_for_work_tree(&cwd)?;
                            let forgotten = store.forget(&operation_id)?;
                            output::print_json(&serde_json::json!({
                                "schema_version": 1,
                                "operation_id": forgotten.operation_id,
                                "forgotten": true,
                                "outcome": "unknown_acknowledged_by_user",
                                "remote_contacted": false,
                                "recommended_next_commands": ["oak agent state --json --compact"]
                            }))?;
                            Ok(())
                        })()
                    }
                }
            },
        },
        Commands::Conflict { action } => match action {
            ConflictCommands::Status { json } => {
                if json {
                    if let Some(dest) = mount_dest_for_cwd(&cwd) {
                        #[cfg(any(
                            target_os = "macos",
                            target_os = "linux",
                            target_os = "windows"
                        ))]
                        {
                            commands::conflict::status_mount(&dest)
                        }
                        #[cfg(not(any(
                            target_os = "macos",
                            target_os = "linux",
                            target_os = "windows"
                        )))]
                        {
                            let _ = dest;
                            commands::conflict::status_checkout(&cwd)
                        }
                    } else {
                        commands::conflict::status_checkout(&cwd)
                    }
                } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
                    {
                        commands::conflict::status_mount_human(&dest)
                    }
                    #[cfg(not(any(
                        target_os = "macos",
                        target_os = "linux",
                        target_os = "windows"
                    )))]
                    {
                        let _ = dest;
                        commands::conflict::status_checkout_human(&cwd)
                    }
                } else {
                    commands::conflict::status_checkout_human(&cwd)
                }
            }
            ConflictCommands::Show { json } => {
                if json {
                    if let Some(dest) = mount_dest_for_cwd(&cwd) {
                        #[cfg(any(
                            target_os = "macos",
                            target_os = "linux",
                            target_os = "windows"
                        ))]
                        {
                            commands::conflict::show_mount(&dest)
                        }
                        #[cfg(not(any(
                            target_os = "macos",
                            target_os = "linux",
                            target_os = "windows"
                        )))]
                        {
                            let _ = dest;
                            commands::conflict::show_checkout(&cwd)
                        }
                    } else {
                        commands::conflict::show_checkout(&cwd)
                    }
                } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
                    {
                        commands::conflict::show_mount_human(&dest)
                    }
                    #[cfg(not(any(
                        target_os = "macos",
                        target_os = "linux",
                        target_os = "windows"
                    )))]
                    {
                        let _ = dest;
                        commands::conflict::show_checkout_human(&cwd)
                    }
                } else {
                    commands::conflict::show_checkout_human(&cwd)
                }
            }
            ConflictCommands::Take {
                path,
                ours,
                theirs,
                json,
            } => {
                let side = match (ours, theirs) {
                    (true, false) => Some(commands::conflict::TakeSide::Ours),
                    (false, true) => Some(commands::conflict::TakeSide::Theirs),
                    _ => None,
                };
                if let Some(side) = side {
                    if let Some(dest) = mount_dest_for_cwd(&cwd) {
                        #[cfg(any(
                            target_os = "macos",
                            target_os = "linux",
                            target_os = "windows"
                        ))]
                        {
                            if json {
                                commands::conflict::take_mount_json(&dest, &path, side)
                            } else {
                                commands::conflict::take_mount(&dest, &path, side)
                            }
                        }
                        #[cfg(not(any(
                            target_os = "macos",
                            target_os = "linux",
                            target_os = "windows"
                        )))]
                        {
                            let _ = dest;
                            if json {
                                commands::conflict::take_checkout_json(&cwd, &path, side)
                            } else {
                                commands::conflict::take_checkout(&cwd, &path, side)
                            }
                        }
                    } else {
                        if json {
                            commands::conflict::take_checkout_json(&cwd, &path, side)
                        } else {
                            commands::conflict::take_checkout(&cwd, &path, side)
                        }
                    }
                } else {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak conflict take` requires exactly one of --ours or --theirs"
                            .to_string(),
                    ))
                }
            }
        },
        Commands::Checkout { reference } => commands::checkout::run(&cwd, &reference),

        Commands::Restore {
            paths,
            source,
            force,
            json,
        } => {
            if json && !force {
                Err(oak_core::OakError::InvalidArgument(
                    "`oak restore --json` cannot prompt for confirmation; pass --force".to_string(),
                ))
            } else {
                if json {
                    output::route_narration_to_stderr();
                }
                commands::restore::run_reporting(&cwd, &paths, source.as_deref(), force).and_then(
                    |outcome| {
                        if json {
                            print_receipt("restore", discard_receipt(&outcome))
                        } else {
                            Ok(())
                        }
                    },
                )
            }
        }

        Commands::Diff(DiffArgs {
            paths,
            against,
            mode,
            json,
            changed_files_limit,
            changed_files_offset,
            hunks,
            max_bytes,
            print,
            stat,
            name_only,
            branch,
            exit_code,
            unified,
            word_diff,
            text,
            check,
            fingerprint,
            verify_fingerprint,
            line_numbers,
            remote,
        }) => {
            if line_numbers && !(print || hunks) {
                Err(oak_core::OakError::InvalidArgument(
                    "--line-numbers requires --print or --json --hunks".to_string(),
                ))
            } else if remote {
                if !(json || print) {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak diff --remote OLD NEW` requires --json or --print".to_string(),
                    ))
                } else {
                    let options = commands::review::DiffJsonOptions {
                        changed_files_limit,
                        changed_files_offset,
                        hunks: hunks || print,
                        max_bytes,
                        context: unified.unwrap_or(oak_core::DEFAULT_CONTEXT_LINES),
                        force_text: text,
                        line_numbers,
                    };
                    match rt.block_on(commands::review::remote_endpoint_diff(
                        &cwd,
                        &paths,
                        positionals_after_double_dash(),
                        json,
                        options,
                    )) {
                        Ok(true) if exit_code => {
                            output::set_exit_status(1);
                            Ok(())
                        }
                        Ok(_) => Ok(()),
                        Err(e) => Err(e),
                    }
                }
            } else if let Some(dest) = mount_dest_for_cwd(&cwd).filter(|_| {
                // Plain working-tree diffs keep the mount's own renderer and
                // JSON (with its `mount` block); every other form — revision
                // endpoints, --branch, hunks, checks, fingerprints — runs the
                // checkout dispatcher over the mount's overlay below.
                let forced = positionals_after_double_dash().unwrap_or(0);
                let maybe_endpoint = paths
                    .iter()
                    .take(paths.len().saturating_sub(forced))
                    .any(|path| !cwd.join(path).exists());
                !(maybe_endpoint
                    || branch
                    || exit_code
                    || against.is_some()
                    || mode.is_some()
                    || hunks
                    || unified.is_some()
                    || word_diff
                    || text
                    || check
                    || fingerprint
                    || verify_fingerprint.is_some()
                    || line_numbers
                    || max_bytes.is_some())
            }) {
                if json {
                    commands::mount::diff_json(
                        &dest,
                        &paths,
                        changed_files_limit,
                        changed_files_offset,
                    )
                } else {
                    commands::mount::diff(&dest, print, &paths, stat, name_only)
                }
            } else {
                let request = commands::diff::DiffRequest {
                    args: paths,
                    forced_path_count: positionals_after_double_dash(),
                    json,
                    json_options: commands::review::DiffJsonOptions {
                        changed_files_limit,
                        changed_files_offset,
                        hunks,
                        max_bytes,
                        context: unified.unwrap_or(oak_core::DEFAULT_CONTEXT_LINES),
                        force_text: text,
                        line_numbers,
                    },
                    print,
                    stat,
                    name_only,
                    branch,
                    against,
                    mode,
                    unified,
                    word_diff,
                    force_text: text,
                    check,
                    fingerprint,
                    verify_fingerprint,
                    line_numbers,
                };
                // `--exit-code`: report "differences exist" as exit 1 without
                // making callers parse any output, matching git's convention.
                // `--check` / `--verify-fingerprint` repurpose the flag as
                // "problems found" / "fingerprint mismatch" (also exit 1).
                let checked_exit =
                    exit_code || request.check || request.verify_fingerprint.is_some();
                match commands::diff::dispatch(&cwd, request) {
                    // Checked exit: report any recorded stdout loss first.
                    Ok(true) if checked_exit => {
                        output::set_exit_status(1);
                        Ok(())
                    }
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                }
            }
        }

        Commands::Status {
            json,
            compact,
            porcelain,
            short,
            reconcile,
            fingerprint,
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                commands::mount::require_live_for_status(&dest).and_then(|_| {
                    if json && fingerprint {
                        commands::mount::status_json_fingerprinted(&dest, &cwd, compact)
                    } else if json && compact {
                        commands::mount::status_compact_json(&dest)
                    } else if json {
                        commands::mount::status_json(&dest)
                    } else if porcelain || short {
                        commands::mount::status_porcelain(&dest)
                    } else {
                        commands::mount::status(&dest)
                    }
                })
            } else if json && reconcile {
                // Reconcile first (its narration goes to stderr), then print
                // the one status document, which reflects the result.
                output::route_narration_to_stderr();
                oak_cli::resolve::resolve(&cwd)
                    .and_then(|_| commands::status::run(&cwd, true))
                    .and_then(|()| commands::status::run_json(&cwd, compact, fingerprint))
            } else if json {
                commands::status::run_json(&cwd, compact, fingerprint)
            } else if porcelain || short {
                commands::status::run_porcelain(&cwd)
            } else {
                commands::status::run(&cwd, reconcile)
            }
        }

        Commands::Info { json } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                if json {
                    commands::mount::info_json(&dest)
                } else {
                    commands::mount::info(&dest)
                }
            } else if json {
                commands::status::run_info_json(&cwd)
            } else {
                commands::status::run_info(&cwd)
            }
        }

        Commands::Hash => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                commands::mount::hash(&dest)
            } else {
                commands::hash::run(&cwd)
            }
        }

        Commands::RevParse { short, rev } => {
            if rev != "HEAD" {
                Err(oak_core::OakError::InvalidArgument(
                    "oak rev-parse compatibility currently supports only HEAD; use `oak hash` for the current commit".to_string(),
                ))
            } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                commands::mount::rev_parse_head(&dest, short)
            } else {
                commands::hash::run_rev_parse_head(&cwd, short)
            }
        }

        Commands::Maintenance { action } => match action {
            MaintenanceCommands::Compact => commands::maintenance::compact(&cwd),
        },

        Commands::Log(LogArgs {
            limit,
            verbose,
            oneline,
            json,
            paths,
            search,
            search_regex,
            remote,
            branch,
            from,
        }) => {
            if remote {
                rt.block_on(commands::log_remote::run_json(
                    &cwd,
                    branch.as_deref(),
                    from.as_deref(),
                    limit,
                ))
            } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                if !paths.is_empty() || search.is_some() || search_regex.is_some() {
                    // Filtered history runs the checkout log over the mount
                    // cache (its virtual branch is the current branch there).
                    if json {
                        commands::log::run_json(
                            &cwd,
                            limit,
                            &paths,
                            search.as_deref(),
                            search_regex.as_deref(),
                        )
                    } else {
                        commands::log::run(
                            &cwd,
                            limit,
                            verbose,
                            oneline,
                            &paths,
                            search.as_deref(),
                            search_regex.as_deref(),
                        )
                    }
                } else if json {
                    commands::mount::log_json(&dest, limit)
                } else {
                    commands::mount::log(&dest, limit, oneline)
                }
            } else if json {
                commands::log::run_json(
                    &cwd,
                    limit,
                    &paths,
                    search.as_deref(),
                    search_regex.as_deref(),
                )
            } else {
                commands::log::run(
                    &cwd,
                    limit,
                    verbose,
                    oneline,
                    &paths,
                    search.as_deref(),
                    search_regex.as_deref(),
                )
            }
        }

        Commands::Push {
            remote,
            force,
            repo,
            json,
            plan,
        } => {
            if plan {
                if let Some(dest) = mount_dest_for_cwd(&cwd) {
                    rt.block_on(commands::mount::push_plan_json(&dest, force))
                } else {
                    rt.block_on(commands::push::plan_json(&cwd, remote.as_deref(), force))
                }
            } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Inside a mount, route to mount push. The mount config
                // already knows the owner/repo/remote, so `remote`, `force`,
                // and `repo` flags from the top-level are ignored here.
                if json {
                    rt.block_on(commands::mount::push_json(&dest))
                } else {
                    rt.block_on(commands::mount::push_with_merge_check(&dest))
                }
            } else {
                if json {
                    rt.block_on(commands::push::run_json(
                        &cwd,
                        remote.as_deref(),
                        force,
                        repo.as_deref(),
                    ))
                } else {
                    rt.block_on(commands::push::run_with_merge_check(
                        &cwd,
                        remote.as_deref(),
                        force,
                        repo.as_deref(),
                    ))
                }
            }
        }

        Commands::Pull {
            remote,
            force,
            r#continue,
            abort,
            json,
            branch_only,
        } => {
            let options = commands::pull::PullOptions { force, branch_only };
            // Inside a mount, pull the trunk into the virtual branch (and drive
            // its conflict-resolution state machine) instead of running the
            // regular working-tree pull, which has no `.oak/` repo to operate on.
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                if (json || branch_only) && (r#continue || abort) {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak pull --json` / `--branch-only` cannot be combined with --continue or --abort"
                            .to_string(),
                    ))
                } else if json || branch_only {
                    rt.block_on(commands::mount::pull::run_receipt(&dest, branch_only, json))
                } else {
                    rt.block_on(commands::mount::pull::run(&dest, force, r#continue, abort))
                }
            } else if json && abort {
                rt.block_on(commands::pull::run_abort_json(&cwd))
            } else if json && r#continue {
                rt.block_on(commands::pull::run_continue_json(&cwd))
            } else if json {
                rt.block_on(commands::pull::run_json(&cwd, remote.as_deref(), options))
            } else {
                rt.block_on(commands::pull::run_with_options(
                    &cwd,
                    remote.as_deref(),
                    options,
                    r#continue,
                    abort,
                ))
            }
        }

        Commands::Fetch { remote, json } => {
            let before = json.then(|| {
                output::route_narration_to_stderr();
                observed_branch_head(&cwd, "main")
            });
            match (
                rt.block_on(commands::fetch::run(&cwd, remote.as_deref())),
                before,
            ) {
                (Ok(()), Some(head_before)) => {
                    let head_after = observed_branch_head(&cwd, "main");
                    let (checkout_branch, _) = observed_branch_and_head(&cwd);
                    print_receipt(
                        "fetch",
                        serde_json::json!({
                            "branch": "main",
                            "updated": head_before != head_after,
                            "head_before": head_before,
                            "head_after": head_after,
                            "checkout_branch": checkout_branch,
                            "recommended_next_commands": [
                                oak_cli::contract::NextCommand::StatusJsonCompact.render()
                            ],
                        }),
                    )
                }
                (outcome, _) => outcome,
            }
        }

        Commands::Reset { path, force, json } => {
            if json && !force {
                Err(oak_core::OakError::InvalidArgument(
                    "`oak reset --json` cannot prompt for confirmation; pass --force".to_string(),
                ))
            } else {
                if json {
                    output::route_narration_to_stderr();
                }
                commands::reset::run_reporting(&cwd, path.as_deref(), force).and_then(|outcome| {
                    if json {
                        print_receipt("reset", discard_receipt(&outcome))
                    } else {
                        Ok(())
                    }
                })
            }
        }

        Commands::Clone(CloneArgs {
            remote,
            name,
            dest,
            branch,
            expected_head,
            shallow,
            allow_unverified_integrity,
            allow_legacy_scope,
            paths,
            json,
            detached,
            from,
            git,
            // `--full` is a deprecated no-op now that full history is the
            // default; accepted only so old invocations keep parsing.
            full: _,
        }) => (|| {
            if from.is_some()
                && (name.is_none()
                    || name
                        .as_deref()
                        .is_some_and(commands::git_clone::looks_like_git_url))
            {
                return Err(oak_core::OakError::InvalidArgument(
                    "clone --from requires an ORG/REPO Oak spec (not the interactive picker or a git URL)"
                        .to_string(),
                ));
            }
            let from = from.map(|from| {
                if from.is_absolute() {
                    from
                } else {
                    cwd.join(from)
                }
            });
            // `oak clone http://host:8080/acme/demo`: the URL carries both the
            // server and the spec. An explicit `-r` must agree with it.
            let (remote, name) = match name.as_deref().map(split_oak_repo_url).transpose()? {
                Some(Some((url_remote, spec))) => {
                    if remote.is_some() {
                        let explicit = remote_arg_or_env_or_default(remote.as_deref())?;
                        if explicit != url_remote {
                            return Err(oak_core::OakError::InvalidArgument(format!(
                                "--remote {explicit} conflicts with the server in the repo URL ({url_remote})"
                            )));
                        }
                    }
                    (url_remote, Some(spec))
                }
                _ => (remote_arg_or_env_or_default(remote.as_deref())?, name),
            };
            // A `--path a,b --path c` flatten: split each value on commas so
            // both repeatable and comma-separated forms work.
            let sparse = oak_core::SparseCone::new(paths.iter().flat_map(|p| p.split(',')));
            if sparse.is_some()
                && (name.is_none()
                    || name
                        .as_deref()
                        .is_some_and(commands::git_clone::looks_like_git_url))
            {
                return Err(oak_core::OakError::InvalidArgument(
                    "clone --path requires an ORG/REPO Oak spec (not the interactive picker or a git URL)"
                        .to_string(),
                    ));
            }
            validate_clone_integrity_override_target(
                name.as_deref(),
                allow_unverified_integrity,
                allow_legacy_scope,
            )?;
            validate_expected_clone_head(branch.as_deref(), detached, expected_head.as_deref())?;
            match name {
                None => {
                    if branch.is_some() {
                        Err(oak_core::OakError::Io(std::io::Error::other(
                            "clone --branch requires ORG/REPO",
                        )))
                    } else {
                        if dest.is_some() {
                            output::warning(
                                "Destination is ignored when using the interactive clone picker",
                            );
                        }
                        if json || detached {
                            Err(oak_core::OakError::InvalidArgument(
                                "clone --json/--detached requires ORG/REPO; the interactive picker is not a machine-readable acquisition target"
                                    .to_string(),
                            ))
                        } else {
                            rt.block_on(commands::repo::clone_interactive_with_policy(
                                &remote,
                                &cwd,
                                shallow,
                                commands::integrity::CloneIntegrityPolicy {
                                    allow_legacy_scope,
                                    allow_unverified_budget: allow_unverified_integrity,
                                },
                            ))
                        }
                    }
                }
                Some(name) => {
                    // If `name` looks like a git remote URL, hand off to the git
                    // converter instead of treating it as an oak `owner/repo` spec.
                    if git || commands::git_clone::looks_like_git_url(&name) {
                        if json {
                            Err(oak_core::OakError::InvalidArgument(
                                "clone --json is only supported for Oak repositories".to_string(),
                            ))
                        } else if branch.is_some() || detached {
                            Err(oak_core::OakError::Io(std::io::Error::other(
                                "clone --branch/--detached is only supported for Oak repositories",
                            )))
                        } else if git {
                            commands::git_remote::clone(&name, dest, &cwd)
                        } else {
                            commands::git_clone::run(&name, dest, &cwd)
                        }
                    } else {
                        // If no destination is given, derive it from the repo name (the
                        // segment after `/` in the spec). Falls back to the spec verbatim
                        // if it doesn't contain a `/` — `repo::clone_repo` will then reject the
                        // spec with a clearer error than a path-related one.
                        let dest = dest.unwrap_or_else(|| {
                            let leaf = name.rsplit('/').next().unwrap_or(&name);
                            PathBuf::from(leaf)
                        });
                        let full_dest = if dest.is_absolute() {
                            dest
                        } else {
                            cwd.join(dest)
                        };
                        let clone = |target: &str, cone: Option<oak_core::SparseCone>| {
                            rt.block_on(commands::repo::clone_repo_with_seed(
                                &remote,
                                target,
                                &full_dest,
                                shallow,
                                cone,
                                branch.as_deref(),
                                expected_head.as_deref(),
                                commands::integrity::CloneIntegrityPolicy {
                                    allow_legacy_scope,
                                    allow_unverified_budget: allow_unverified_integrity,
                                },
                                json,
                                detached,
                                from.as_deref(),
                            ))
                        };
                        // `--path` against a server that serves partial
                        // checkouts of this repo: a view, which leaves the
                        // other directories out of the tree entirely rather
                        // than only withholding their content.
                        let view = match sparse.as_ref() {
                            Some(cone) => (|| {
                                let target =
                                    commands::access::RepoTarget::for_spec(&remote, &name)?;
                                if !rt.block_on(commands::access::serves_views(&target)) {
                                    return None;
                                }
                                let spec = oak_core::ViewSpec::new(
                                    cone.prefixes(),
                                    [] as [&str; 0],
                                    oak_core::view::DEFAULT_SPINE_FILES,
                                )
                                .ok()?;
                                Some(oak_cli::http::ActiveView {
                                    repo: target.slug(),
                                    spec: spec.to_canonical(),
                                })
                            })(),
                            // No `--path`, but the repo has directories this
                            // caller can't read: the server serves them `main`
                            // only as a view, so clone the whole repo as one
                            // and the server leaves those directories out.
                            None => (|| {
                                let target =
                                    commands::access::RepoTarget::for_spec(&remote, &name)?;
                                let hidden = rt
                                    .block_on(commands::access::hidden_directories(&target))
                                    .ok()??;
                                if hidden == 0 {
                                    return None;
                                }
                                let spec = oak_core::ViewSpec::new(
                                    [""],
                                    [] as [&str; 0],
                                    oak_core::view::DEFAULT_SPINE_FILES,
                                )
                                .ok()?;
                                Some(oak_cli::http::ActiveView {
                                    repo: target.slug(),
                                    spec: spec.to_canonical(),
                                })
                            })(),
                        };
                        match view {
                            Some(view) => {
                                oak_cli::http::set_active_view(Some(view.clone()));
                                clone(&name, None)
                                    .and_then(|()| commands::view::record_view(&full_dest, &view))
                            }
                            None => clone(&name, sparse.clone()),
                        }
                    }
                }
            }
        })(),

        Commands::Doctor {
            repo,
            remote,
            verify,
            depth,
            max_chunks,
            max_bytes,
            json,
        } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            rt.block_on(commands::integrity::doctor(
                &remote, &repo, verify, depth, max_chunks, max_bytes, json,
            ))
        })(),

        Commands::Blob { action } => match action {
            BlobCommands::Info {
                hash,
                repo,
                remote,
                depth,
                branch,
                json,
            } => (|| {
                let remote = remote_arg_or_env_or_default(remote.as_deref())?;
                rt.block_on(commands::integrity::blob_info_scoped(
                    &remote,
                    &repo,
                    &hash,
                    depth,
                    branch.as_deref(),
                    json,
                ))
            })(),
        },

        Commands::File {
            action:
                FileCommands::Inspect {
                    at,
                    path,
                    max_bytes,
                    json,
                    remote,
                    output: output_file,
                },
        } => match if remote {
            rt.block_on(commands::file::inspect_remote(
                &cwd, &at, &path, max_bytes, json,
            ))
        } else {
            commands::file::inspect(&cwd, &at, &path, max_bytes, output_file.as_deref(), json)
        } {
            Ok(true) => Ok(()),
            Ok(false) => {
                output::set_exit_status(1);
                Ok(())
            }
            Err(error) => Err(error),
        },

        Commands::Refs {
            action: RefsCommands::Inspect { max_branches, json },
        } => match commands::refs::inspect(&cwd, max_branches, json) {
            Ok(true) => Ok(()),
            Ok(false) => {
                output::set_exit_status(1);
                Ok(())
            }
            Err(error) => Err(error),
        },

        Commands::Tree {
            action:
                TreeCommands::Inspect {
                    at,
                    max_files,
                    max_bytes,
                    json,
                },
        } => match commands::tree::inspect(
            &cwd,
            &at,
            oak_core::sqlite::tree_inspect::TreeInspectionLimits {
                max_files,
                max_bytes,
            },
            json,
        ) {
            Ok(true) => Ok(()),
            Ok(false) => {
                output::set_exit_status(1);
                Ok(())
            }
            Err(error) => Err(error),
        },

        Commands::Environment { json } => commands::environment::run(&cwd, json),

        Commands::Change { action } => match action {
            ChangeCommands::Capture { paths, json } => {
                commands::change::capture(&cwd, &paths, json)
            }
            ChangeCommands::Export {
                capture_id,
                output,
                json,
            } => commands::change::export(&cwd, &capture_id, &output, json),
        },

        Commands::Site { action } => (|| {
            let (target, action, confirmed) = action.into_action()?;
            if let commands::site::SiteAction::DomainRm { name, domain } = &action {
                if !confirmed {
                    if target.json || !output::can_prompt() {
                        return Err(oak_core::OakError::InvalidArgument(format!(
                            "removing a domain stops it serving the site; re-run with --yes to remove {domain} from '{name}'"
                        )));
                    }
                    output::print_raw(&format!(
                        "Remove {domain} from {name}? It stops serving the site, and adding it back needs a new TXT record. [y/N] "
                    ));
                    let mut answer = String::new();
                    std::io::stdin().read_line(&mut answer)?;
                    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        output::info("Cancelled; nothing was removed.");
                        return Ok(());
                    }
                }
            }
            if let commands::site::SiteAction::Rm { name } = &action {
                if !confirmed {
                    if target.json || !output::can_prompt() {
                        return Err(oak_core::OakError::InvalidArgument(format!(
                            "deleting a site takes its URL offline; re-run with --yes to delete '{name}'"
                        )));
                    }
                    output::print_raw(&format!(
                        "Delete site {name}? Its URL stops working and the name can be taken. [y/N] "
                    ));
                    let mut answer = String::new();
                    std::io::stdin().read_line(&mut answer)?;
                    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        output::info("Cancelled; nothing was deleted.");
                        return Ok(());
                    }
                }
            }
            let remote = remote_arg_or_env(target.remote.as_deref())?;
            match rt.block_on(commands::site::run(
                &cwd,
                target.repo.as_deref(),
                remote.as_deref(),
                target.json,
                action,
            ))? {
                0 => Ok(()),
                code => {
                    output::set_exit_status(code);
                    Ok(())
                }
            }
        })(),

        Commands::Root { action } => (|| {
            let (target, action, confirmed) = action.into_action();
            if let commands::root::RootAction::Archive { root } = &action {
                if !confirmed {
                    let name = commands::root::display_name(root);
                    if target.json || !output::can_prompt() {
                        return Err(oak_core::OakError::InvalidArgument(format!(
                            "archiving a root stops it taking pushes; re-run with --yes to archive {name}"
                        )));
                    }
                    output::print_raw(&format!(
                        "Archive {name}? Its history stays readable but it takes no more pushes. [y/N] "
                    ));
                    let mut answer = String::new();
                    std::io::stdin().read_line(&mut answer)?;
                    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        output::info("Cancelled; nothing was archived.");
                        return Ok(());
                    }
                }
            }
            let remote = remote_arg_or_env(target.remote.as_deref())?;
            match rt.block_on(commands::root::run(
                &cwd,
                target.repo.as_deref(),
                remote.as_deref(),
                target.json,
                action,
            ))? {
                0 => Ok(()),
                code => {
                    output::set_exit_status(code);
                    Ok(())
                }
            }
        })(),

        Commands::Sparse { action, json } => (|| {
            use commands::sparse::SparseAction;
            let action = match action {
                None => SparseAction::List { json },
                Some(SparseCommands::List { json: sub }) => {
                    SparseAction::List { json: json || sub }
                }
                Some(SparseCommands::Set { paths, .. }) => SparseAction::Set { paths },
                Some(SparseCommands::Add { paths, .. }) => SparseAction::Add { paths },
                Some(SparseCommands::Remove { paths, .. }) => SparseAction::Remove { paths },
                Some(SparseCommands::Disable { .. }) => SparseAction::Disable,
            };
            // `--json` on the subcommand or on `oak sparse` itself; decided
            // once for the invocation.
            let json = json_mode;
            // A view is this server's sparse checkout: act on it.
            if commands::view::is_view_checkout(&cwd) {
                use commands::view::ViewAction;
                let split = |paths: Vec<String>| -> Vec<String> {
                    paths
                        .iter()
                        .flat_map(|p| p.split(','))
                        .filter(|p| !p.trim().is_empty())
                        .map(str::to_string)
                        .collect()
                };
                let (action, json) = match action {
                    SparseAction::List { json } => (ViewAction::Show, json),
                    SparseAction::Add { paths } => (
                        ViewAction::Add {
                            paths: split(paths),
                        },
                        json,
                    ),
                    SparseAction::Remove { paths } => (
                        ViewAction::Remove {
                            paths: split(paths),
                        },
                        json,
                    ),
                    SparseAction::Set { paths } => (ViewAction::Set { paths }, json),
                    SparseAction::Disable => {
                        return Err(oak_core::OakError::InvalidArgument(
                            "this is a partial checkout from the server: add directories with \
                             `oak sparse add`, or `oak clone` the repo without --path for a \
                             full checkout"
                                .to_string(),
                        ))
                    }
                };
                return commands::view::run(&cwd, &rt, action, json);
            }
            if json && !matches!(action, SparseAction::List { .. }) {
                // A change to the cone: narrate on stderr, then report the
                // cone as stored — the same document `oak sparse list --json`
                // prints.
                output::route_narration_to_stderr();
                commands::sparse::run(&cwd, action)?;
                return commands::sparse::run(&cwd, SparseAction::List { json: true });
            }
            commands::sparse::run(&cwd, action)
        })(),

        Commands::Retention {
            action,
            repo,
            remote,
            json,
        } => (|| {
            let target =
                commands::access::RepoTarget::resolve(&cwd, repo.as_deref(), remote.as_deref())?;
            match action.unwrap_or(RetentionCommands::List) {
                RetentionCommands::List => rt.block_on(commands::retention::list(target, json)),
                RetentionCommands::Set { path, keep } => {
                    rt.block_on(commands::retention::set(target, &path, keep, json))
                }
                RetentionCommands::Remove { path } => {
                    rt.block_on(commands::retention::remove(target, &path, json))
                }
            }
        })(),

        Commands::Access { repo, remote, json } => (|| {
            let target =
                commands::access::RepoTarget::resolve(&cwd, repo.as_deref(), remote.as_deref())?;
            rt.block_on(commands::access::access(target, json))
        })(),

        Commands::Owners {
            path,
            branch,
            repo,
            remote,
            json,
        } => (|| {
            if branch {
                return rt.block_on(commands::access::branch_impact(&cwd, json));
            }
            let target =
                commands::access::RepoTarget::resolve(&cwd, repo.as_deref(), remote.as_deref())?;
            rt.block_on(commands::access::owners(
                target,
                path.as_deref().unwrap_or_default(),
                json,
            ))
        })(),

        Commands::Groups { org, remote, json } => (|| {
            let remote = match remote.as_deref() {
                Some(_) => remote_arg_or_env_or_default(remote.as_deref())?,
                None => commands::access::RepoTarget::resolve(&cwd, None, None)
                    .map(|t| t.remote)
                    .or_else(|_| remote_arg_or_env_or_default(None))?,
            };
            rt.block_on(commands::group::mine(remote, org.as_deref(), &cwd, json))
        })(),

        Commands::Group {
            action: GroupCommands::Switch { slug, clear },
            json,
            ..
        } => rt.block_on(commands::group::switch(&cwd, slug, clear, json)),

        Commands::Group {
            action,
            org,
            remote,
            json,
        } => (|| {
            use commands::group::GroupAction;
            let (org, remote) = match org {
                Some(org) => (org, remote_arg_or_env_or_default(remote.as_deref())?),
                None => {
                    let target =
                        commands::access::RepoTarget::resolve(&cwd, None, None).map_err(|_| {
                            oak_core::OakError::InvalidArgument(
                                "name the org with --org ORG (or run this in a checkout)"
                                    .to_string(),
                            )
                        })?;
                    let remote = match remote.as_deref() {
                        Some(_) => remote_arg_or_env_or_default(remote.as_deref())?,
                        None => target.remote.clone(),
                    };
                    (target.owner, remote)
                }
            };
            let action = match action {
                GroupCommands::List => GroupAction::List,
                GroupCommands::Show { slug } => GroupAction::Show { slug },
                GroupCommands::Create {
                    slug,
                    parent,
                    description,
                } => GroupAction::Create {
                    slug,
                    parent,
                    description,
                },
                GroupCommands::Add {
                    slug,
                    username,
                    maintainer,
                } => GroupAction::Add {
                    slug,
                    username,
                    maintainer,
                },
                GroupCommands::Remove { slug, username } => GroupAction::Remove { slug, username },
                GroupCommands::Invite {
                    slug,
                    username,
                    maintainer,
                } => GroupAction::Invite {
                    slug,
                    username,
                    maintainer,
                },
                GroupCommands::Grant { slug, repo, role } => {
                    GroupAction::Grant { slug, repo, role }
                }
                GroupCommands::Revoke { slug, repo } => GroupAction::Revoke { slug, repo },
                GroupCommands::Agent { action } => match action {
                    GroupAgentCommands::List => GroupAction::AgentList,
                    GroupAgentCommands::Add { slug, agent } => {
                        GroupAction::AgentAdd { slug, agent }
                    }
                    GroupAgentCommands::Remove { slug, agent } => {
                        GroupAction::AgentRemove { slug, agent }
                    }
                },
                GroupCommands::Switch { .. } => unreachable!("handled above"),
            };
            rt.block_on(commands::group::run(remote, &org, action, json))
        })(),

        Commands::Ls {
            path,
            remote,
            rev,
            max_entries,
            json,
        } => require_remote_flag("ls", remote).and_then(|()| {
            rt.block_on(commands::remote_read::ls_remote(
                &cwd,
                &rev,
                &path,
                max_entries.max(1),
                json,
            ))
        }),

        Commands::Cat {
            path,
            remote,
            rev,
            max_bytes,
            json,
        } => match require_remote_flag("cat", remote).and_then(|()| {
            rt.block_on(commands::file::inspect_remote(
                &cwd, &rev, &path, max_bytes, json,
            ))
        }) {
            Ok(true) => Ok(()),
            Ok(false) => {
                output::set_exit_status(1);
                Ok(())
            }
            Err(error) => Err(error),
        },

        Commands::Grep {
            pattern,
            paths,
            remote,
            regex,
            case_sensitive,
            max_files,
            json,
        } => match require_remote_flag("grep", remote).and_then(|()| {
            rt.block_on(commands::remote_read::grep_remote(
                &cwd,
                commands::remote_read::GrepOptions {
                    pattern,
                    paths,
                    regex,
                    case_sensitive,
                    max_files: max_files.max(1),
                },
                json,
            ))
        }) {
            Ok(true) => Ok(()),
            Ok(false) => {
                output::set_exit_status(1);
                Ok(())
            }
            Err(error) => Err(error),
        },

        Commands::Archive { output } => commands::archive::run(&cwd, output.as_deref()),

        Commands::Approve {
            branch,
            branch_flag,
            boundary,
            head,
            repo,
            remote,
            json,
        } => rt.block_on(commands::approve::run(
            &cwd,
            &commands::branch_target::TargetArgs {
                repo,
                branch: branch.or(branch_flag),
                remote,
            },
            boundary.as_deref(),
            head.as_deref(),
            json,
        )),

        Commands::Ci { action } => match action {
            CiCommands::Trigger {
                expected_commit,
                idempotency_key,
                branch,
                workflow,
                json,
            } => rt.block_on(commands::ci::trigger(
                &cwd,
                branch.as_deref(),
                &expected_commit,
                &idempotency_key,
                workflow.as_deref(),
                json,
            )),
            CiCommands::Runs { limit, json } => rt.block_on(commands::ci::runs(&cwd, limit, json)),
            CiCommands::Status {
                json,
                head,
                run,
                commit,
            } => match rt.block_on(async {
                if head || run.is_some() {
                    commands::ci::status_with_run(&cwd, json, run, commit.as_deref()).await
                } else {
                    commands::ci::services_status(&cwd, json).await
                }
            }) {
                // `oak ci status` communicates its state through its exit
                // code (0 success / 1 failure / 3 running) so scripts can
                // branch without parsing output.
                Ok(0) => Ok(()),
                // Checked exit: report any recorded stdout loss first.
                Ok(code) => {
                    output::set_exit_status(code);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            CiCommands::Wait {
                run_ids,
                current,
                dispatch_timeout,
                commit,
                timeout,
                json,
                summary,
                progress,
                events,
            } => {
                let options = commands::ci::WaitOptions {
                    timeout: std::time::Duration::from_secs(timeout),
                    json,
                    summary,
                    progress: if events {
                        commands::ci::ProgressMode::Jsonl
                    } else if progress {
                        commands::ci::ProgressMode::Text
                    } else {
                        commands::ci::ProgressMode::Off
                    },
                };
                let result = if dispatch_timeout.is_some() && !current {
                    Err(oak_core::OakError::InvalidArgument(
                        "--dispatch-timeout requires --current".into(),
                    ))
                } else if current {
                    rt.block_on(commands::ci::wait_current(
                        &cwd,
                        options,
                        std::time::Duration::from_secs(
                            dispatch_timeout.unwrap_or(commands::ci::DEFAULT_DISPATCH_TIMEOUT_SECS),
                        ),
                    ))
                } else {
                    rt.block_on(commands::ci::wait_with_options(
                        &cwd,
                        &run_ids,
                        commit.as_deref(),
                        options,
                    ))
                };
                match result {
                    Ok(0) => Ok(()),
                    Ok(code) => {
                        output::set_exit_status(code);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            CiCommands::Logs {
                run_id,
                json,
                summary,
                failed,
                max_bytes,
            } => rt.block_on(commands::ci::logs_with_options(
                &cwd,
                run_id,
                json,
                commands::ci::LogOptions {
                    summary,
                    failed,
                    max_bytes,
                },
            )),
            CiCommands::Rerun { run_id, json } => {
                rt.block_on(commands::ci::rerun(&cwd, run_id, json))
            }
            CiCommands::Cancel {
                superseded: true,
                yes,
                json,
                ..
            } => match rt.block_on(commands::ci::cancel_superseded(&cwd, yes, json)) {
                Ok(0) => Ok(()),
                Ok(code) => {
                    output::set_exit_status(code);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            CiCommands::Cancel {
                run_id,
                commit,
                json,
                dry_run,
                yes,
                ..
            } => match (run_id, commit) {
                _ if dry_run || yes => Err(oak_core::OakError::InvalidArgument(
                    "--dry-run and --yes require --superseded".into(),
                )),
                (Some(run_id), Some(commit)) => {
                    rt.block_on(commands::ci::cancel(&cwd, run_id, &commit, json))
                }
                _ => Err(oak_core::OakError::InvalidArgument(
                    "oak ci cancel requires RUN_ID and --commit HASH (or --superseded)".into(),
                )),
            },
            CiCommands::Approve {
                run_id,
                commit,
                json,
            } => rt.block_on(commands::ci::approve(&cwd, run_id, &commit, json)),
            CiCommands::Deploy {
                target,
                environment,
                workflow,
                branch,
                commit,
                pause_auto_publish,
                yes,
                dry_run,
                wait,
                json,
            } => match rt.block_on(commands::ci::deploy::deploy(
                &cwd,
                &commands::ci::deploy::DeployOptions {
                    environment: commands::ci::deploy::publish_key(
                        target.or(environment).as_deref().unwrap_or_default(),
                    ),
                    workflow,
                    branch,
                    commit,
                    pause_auto_publish,
                    yes,
                    dry_run,
                    wait_minutes: wait,
                    json,
                },
            )) {
                // The outcome is carried by the exit code (see `oak ci
                // deploy --help`), so scripts can branch without parsing.
                Ok(0) => Ok(()),
                Ok(code) => {
                    output::set_exit_status(code);
                    Ok(())
                }
                Err(e) => Err(e),
            },
        },

        Commands::Export {
            dest,
            tree_only: true,
            at,
            max_files,
            max_bytes,
            json,
            ..
        } => commands::tree::export(
            &cwd,
            at.as_deref().unwrap_or("HEAD"),
            &dest,
            oak_core::sqlite::tree_inspect::TreeInspectionLimits {
                max_files,
                max_bytes,
            },
            json,
        ),
        Commands::Export {
            dest,
            branch,
            git_branch,
            force,
            ..
        } => commands::export::run(&cwd, &dest, branch.as_deref(), git_branch.as_deref(), force),

        Commands::Upgrade {
            force,
            channel,
            canary,
            auto,
        } => rt.block_on(async {
            if auto {
                commands::upgrade::run_auto().await
            } else {
                commands::upgrade::run(force, canary, channel).await
            }
        }),

        Commands::Login { remote } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            commands::login::run(&remote)
        })(),

        Commands::Logout { remote } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            commands::logout::run(&remote)
        })(),

        Commands::Mcp { allow_write } => commands::mcp::run(allow_write),

        Commands::Whoami { remote, json } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            commands::whoami::run(&remote, json)
        })(),

        Commands::Repo {
            action:
                RepoCommands::List {
                    org,
                    limit,
                    json,
                    remote,
                },
        } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            let org = org
                .map(|org| {
                    let org = org.trim().to_string();
                    if org.is_empty()
                        || org
                            .chars()
                            .any(|c| c == '/' || c == '\\' || c.is_whitespace() || c.is_control())
                    {
                        Err(oak_core::OakError::InvalidArgument(format!(
                            "Expected an org slug for --org (e.g. `oak`), got '{org}'"
                        )))
                    } else {
                        Ok(org)
                    }
                })
                .transpose()?;
            if json {
                rt.block_on(commands::repo::list_json(
                    &remote,
                    org.as_deref(),
                    limit as usize,
                ))
                .and_then(|listing| output::print_json(&listing))
            } else {
                rt.block_on(commands::repo::list(
                    &remote,
                    org.as_deref(),
                    limit as usize,
                ))
            }
        })(),

        Commands::Repo {
            action:
                RepoCommands::Visibility {
                    visibility,
                    repo,
                    json,
                    remote,
                },
        } => (|| {
            let remote = remote_arg_or_env(remote.as_deref())?;
            let result = rt.block_on(commands::repo_visibility::run(
                &cwd,
                repo.as_deref(),
                remote.as_deref(),
                visibility,
            ))?;
            if json {
                output::print_json(&result)
            } else {
                commands::repo_visibility::print_human(&result);
                Ok(())
            }
        })(),

        Commands::Repo {
            action:
                RepoCommands::Move {
                    source,
                    into,
                    owner_team,
                    dry_run,
                    no_wait,
                    json,
                    remote,
                },
        } => (|| {
            rt.block_on(commands::repo_move::move_repo(
                remote_arg_or_env_or_default(remote.as_deref())?,
                &source,
                &into,
                &owner_team,
                dry_run,
                !no_wait,
                json,
            ))
        })(),

        Commands::Repo {
            action: RepoCommands::Adopt { into, json },
        } => rt.block_on(commands::repo_move::adopt(&cwd, into, json)),

        Commands::Auth {
            action: AuthCommands::Status { json, remote },
        } => (|| {
            let explicit = match remote.as_deref() {
                Some(remote) => Some((
                    commands::push::normalize_remote_url(remote).ok_or_else(|| {
                        oak_core::OakError::InvalidArgument(
                            "remote URL cannot be empty".to_string(),
                        )
                    })?,
                    "argument",
                )),
                None => commands::push::env_remote_override().map(|remote| (remote, "env")),
            };
            let (remote, source) = commands::auth_status::resolve_remote(explicit, &cwd);
            let status = rt.block_on(commands::auth_status::status(&remote, &source, &cwd))?;
            if json {
                output::print_json(&status)
            } else {
                commands::auth_status::print_human(&status);
                Ok(())
            }
        })(),

        Commands::Open { print, json, site } => {
            let site = site.as_deref();
            if json {
                commands::open::run_json(&cwd, site)
            } else if print {
                commands::open::run_print(&cwd, site)
            } else {
                commands::open::run(&cwd, site)
            }
        }

        // `--remote`/`--json` are combined from both positions, so
        // `oak feedback --json --remote X link fb-1` and
        // `oak feedback link fb-1 --json --remote X` are identical.
        Commands::Feedback {
            action:
                Some(FeedbackCommands::List {
                    status,
                    limit,
                    cursor,
                    remote,
                    json,
                }),
            remote: parent_remote,
            json: parent_json,
            ..
        } => rt.block_on(commands::feedback::inventory::list(
            &cwd,
            status,
            limit,
            cursor,
            remote.or(parent_remote),
            json || parent_json,
        )),

        Commands::Feedback {
            action:
                Some(FeedbackCommands::Transition {
                    item,
                    expected_revision,
                    status,
                    notes,
                    clear_notes,
                    remote,
                    json,
                }),
            remote: parent_remote,
            json: parent_json,
            ..
        } => rt.block_on(commands::feedback::transition::run(
            &cwd,
            commands::feedback::transition::Options {
                item,
                expected_revision,
                status,
                notes,
                clear_notes,
                remote: remote.or(parent_remote),
                json: json || parent_json,
            },
        )),

        Commands::Feedback {
            action: Some(FeedbackCommands::Show { item, remote, json }),
            remote: parent_remote,
            json: parent_json,
            ..
        } => rt.block_on(commands::feedback::inventory::show(
            &cwd,
            item,
            remote.or(parent_remote),
            json || parent_json,
        )),

        Commands::Feedback {
            action:
                Some(FeedbackCommands::Link {
                    item,
                    branch,
                    repo,
                    commit,
                    remote,
                    json,
                }),
            remote: parent_remote,
            json: parent_json,
            ..
        } => rt.block_on(commands::feedback::link(
            &cwd,
            commands::feedback::FeedbackLinkOptions {
                item,
                branch,
                repo,
                commit,
                remote: remote.or(parent_remote),
                json: json || parent_json,
            },
        )),

        Commands::Feedback {
            action:
                Some(FeedbackCommands::Unlink {
                    item,
                    link_id,
                    commit,
                    branch,
                    repo,
                    remote,
                    json,
                }),
            remote: parent_remote,
            json: parent_json,
            ..
        } => rt.block_on(commands::feedback::unlink(
            &cwd,
            commands::feedback::FeedbackUnlinkOptions {
                item,
                link_id,
                commit,
                branch,
                repo,
                remote: remote.or(parent_remote),
                json: json || parent_json,
            },
        )),

        Commands::Feedback {
            action: None,
            message,
            file,
            email,
            name,
            title,
            remote,
            json,
        } => rt.block_on(commands::feedback::run(
            &cwd,
            commands::feedback::FeedbackOptions {
                message,
                file,
                email,
                name,
                title,
                remote,
                json,
            },
        )),

        Commands::Completions { shell } => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let bin_name = cmd.get_name().to_string();
            // Render to a buffer and emit through the broken-pipe-safe funnel:
            // clap_complete writes stdout directly and panics if the reader
            // has gone away (e.g. `oak completions zsh | head`).
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut cmd, bin_name, &mut script);
            output::print_raw(&String::from_utf8_lossy(&script));
            Ok(())
        }

        Commands::Split {
            from,
            plan,
            dry_run,
            json,
        } => {
            if json {
                output::route_narration_to_stderr();
            }
            commands::split::run_reporting(&cwd, from.as_deref(), plan.as_deref(), dry_run)
                .and_then(|outcome| {
                    if !json {
                        return Ok(());
                    }
                    print_receipt(
                        "split",
                        serde_json::json!({
                            "branch": outcome.branch,
                            "dry_run": dry_run,
                            "written": outcome.written,
                            "branches": outcome
                                .branches
                                .iter()
                                .map(|(name, head)| serde_json::json!({"name": name, "head": head}))
                                .collect::<Vec<_>>(),
                            "recommended_next_commands": [
                                oak_cli::contract::NextCommand::StatusJsonCompact.render()
                            ],
                        }),
                    )
                })
        }

        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        Commands::Mount {
            action,
            spec,
            dest,
            branch,
            paths,
            remote,
        } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            match action {
                Some(MountCommands::List { json }) => commands::mount::list(json),
                Some(MountCommands::Finish {
                    dest,
                    desc_file,
                    json,
                }) => read_desc_input("oak mount finish", None, Some(desc_file)).and_then(
                    |description| {
                        let target = dest.unwrap_or_else(|| cwd.clone());
                        let abs_dest = if target.is_absolute() {
                            target
                        } else {
                            cwd.join(target)
                        };
                        if json {
                            rt.block_on(commands::mount::finish_json(&abs_dest, &description))
                                .and_then(|result| output::print_json(&result))
                        } else {
                            rt.block_on(commands::mount::finish(&abs_dest, &description))
                        }
                    },
                ),
                Some(MountCommands::End { dest, force }) => match dest {
                    Some(dest) => {
                        let abs_dest = if dest.is_absolute() {
                            dest
                        } else {
                            cwd.join(dest)
                        };
                        commands::mount::end(&abs_dest, force)
                    }
                    None => commands::mount::end_all(force),
                },
                Some(MountCommands::Forget {
                    dest,
                    force,
                    orphaned,
                }) => match dest {
                    Some(dest) if !orphaned => {
                        let abs_dest = if dest.is_absolute() {
                            dest
                        } else {
                            cwd.join(dest)
                        };
                        commands::mount::forget(&abs_dest, force)
                    }
                    _ => commands::mount::forget_orphaned(),
                },
                Some(MountCommands::Serve {
                    remote,
                    spec,
                    dest,
                    branch,
                    view,
                }) => (|| {
                    let remote = remote_arg_or_env_or_default(remote.as_deref())?;
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    rt.block_on(commands::mount::serve(
                        &remote,
                        &spec,
                        &abs_dest,
                        branch.as_deref(),
                        view.as_deref(),
                    ))
                })(),
                Some(MountCommands::Resume { dest }) => {
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    rt.block_on(commands::mount::serve_resume(&abs_dest))
                }
                Some(MountCommands::WorktreeCreate { remote, spec }) => (|| {
                    let remote = remote_arg_or_env_or_default(remote.as_deref())?;
                    let (spec, view, _) =
                        rt.block_on(commands::mount::resolve_mount_target(&remote, &spec, &[]))?;
                    commands::mount::worktree::worktree_create(&remote, &spec, view.as_deref())
                })(),
                Some(MountCommands::WorktreeRemove) => commands::mount::worktree::worktree_remove(),
                #[cfg(target_os = "macos")]
                Some(MountCommands::FskitBroker) => commands::mount::fskit::broker::run(),
                #[cfg(not(target_os = "macos"))]
                Some(MountCommands::FskitBroker) => Err(oak_core::OakError::Unsupported {
                    backend: "non-macOS",
                    op: "__fskit-broker",
                }),
                #[cfg(target_os = "macos")]
                Some(MountCommands::FskitBrokerPing { text }) => {
                    commands::mount::fskit::broker::ping(&text).map_err(oak_core::OakError::Server)
                }
                #[cfg(not(target_os = "macos"))]
                Some(MountCommands::FskitBrokerPing { .. }) => {
                    Err(oak_core::OakError::Unsupported {
                        backend: "non-macOS",
                        op: "__fskit-broker-ping",
                    })
                }
                None => {
                    // No subcommand: the spec names a single repo to mount.
                    //   (none)                → print help
                    //   <repo> [dest]         → one repo in your org, at <dest>
                    //   <owner>/<repo> [dest] → one repo at <dest> (or ./<repo>)
                    match spec {
                        None if branch.is_some() => Err(oak_core::OakError::InvalidArgument(
                            "--branch needs a repo to mount: oak mount <org>/<repo> --branch <name> [dest]"
                                .into(),
                        )),
                        None => {
                            // Bare `oak mount` prints the subcommand help: a
                            // mount target must be named explicitly. Render it
                            // rather than letting clap `print_help()` straight
                            // to stdout — clap's own writer bypasses the output
                            // funnel, so a broken pipe or a genuine write
                            // failure would be swallowed by its discarded
                            // `io::Result` instead of reaching the epilogue's
                            // stdout-health check.
                            use clap::CommandFactory;
                            let mut app = Cli::command();
                            let rendered = match app.find_subcommand_mut("mount") {
                                Some(sub) => sub.render_help(),
                                None => app.render_help(),
                            };
                            output::print_raw(&output::clap_message_text(
                                &rendered,
                                output::stdout_colors_enabled(),
                            ));
                            Ok(())
                        }
                        Some(spec) => {
                            // Resolve a bare `<repo>` to `<username>/<repo>` from
                            // the logged-in identity, exactly like `oak clone`;
                            // `<org>/<repo>` is used verbatim.
                            match commands::repo::resolve_repo_spec(&remote, &spec) {
                                Ok(resolved) => {
                                    let abs_dest =
                                        dest.map(|d| if d.is_absolute() { d } else { cwd.join(d) });
                                    rt.block_on(commands::mount::mount_one(
                                        &remote,
                                        &resolved,
                                        abs_dest,
                                        &cwd,
                                        branch.as_deref(),
                                        &paths,
                                    ))
                                }
                                Err(e) => Err(e),
                            }
                        }
                    }
                }
            }
        })(),

        Commands::Skill { action } => match action {
            SkillCommands::Install { global } => commands::skill::install(global, &cwd),
        },

        Commands::Start(StartArgs {
            summary,
            name,
            no_push,
            no_watch,
            clean,
            json,
        }) => rt.block_on(commands::start::run(
            &cwd,
            &summary,
            name.as_deref(),
            commands::start::StartOptions {
                no_push,
                no_watch,
                clean,
            },
            json,
        )),

        Commands::Watch {
            action,
            foreground,
            json,
        } => match action {
            None => rt.block_on(commands::watch::start(&cwd, foreground, json)),
            Some(WatchCommands::Start { foreground, json }) => {
                rt.block_on(commands::watch::start(&cwd, foreground, json))
            }
            Some(WatchCommands::Stop { json }) => rt.block_on(commands::watch::stop(&cwd, json)),
            Some(WatchCommands::Status { json }) => commands::watch::status(&cwd, json),
            Some(WatchCommands::Hook { event }) => {
                // A hook must never fail the agent's turn.
                let _ = rt.block_on(commands::watch::hook::run(&event, &cwd));
                Ok(())
            }
        },

        Commands::Config { action } => match action {
            ConfigCommands::Get { key, global, json } => {
                commands::config::get(&cwd, &key, global, json)
            }
            ConfigCommands::Set {
                key,
                value,
                global,
                json,
            } => commands::config::set(&cwd, &key, Some(&value), global, json),
            ConfigCommands::Unset { key, global, json } => {
                commands::config::set(&cwd, &key, None, global, json)
            }
            ConfigCommands::List { global, json } => commands::config::list(&cwd, global, json),
        },
    };

    // A command that succeeded but answers through its exit code (`oak diff
    // --exit-code`, `oak ci wait`) asked for that code with
    // `output::set_exit_status`. Leave through the checked exit: a recorded
    // stdout loss is reported, and the nonzero code keeps its meaning.
    if result.is_ok() && output::exit_status() != 0 {
        output::exit_process(output::exit_status());
    }

    // A non-BrokenPipe stdout write failure (EIO, disk full behind a
    // redirect) means output was lost even if the operation succeeded;
    // surface it through the normal error path. A vanished pipe reader is
    // NOT an error — see `output::write_stdout`.
    let result = result.and_then(|()| output::stdout_write_result());

    if let Err(e) = result {
        exit_with_error(&e, json_mode, Some(&cwd), branch_remote_retry);
    }

    // On the success path, opportunistically check (at most once a day) whether
    // a newer oak is available and, if so, prompt the user to run `oak upgrade`.
    if !skip_version_notice {
        commands::version_check::maybe_notify(&rt);
    }
}

fn remote_arg_or_env(remote: Option<&str>) -> oak_core::Result<Option<String>> {
    let explicit = match remote {
        Some(remote) => Some(commands::push::normalize_remote_url(remote).ok_or_else(|| {
            oak_core::OakError::InvalidArgument("remote URL cannot be empty".to_string())
        })?),
        None => None,
    };
    Ok(explicit.or_else(commands::push::env_remote_override))
}

/// The view the checkout at `cwd` holds, if it is a view checkout. Read-only
/// and best-effort: anything that isn't a readable checkout has no view.
fn checkout_view(cwd: &std::path::Path) -> Option<oak_cli::http::ActiveView> {
    let ctx = oak_cli::resolve::resolve(cwd).ok()?;
    let db = ctx.db_path().ok()?;
    if !db.exists() {
        return None;
    }
    let repo = oak_core::SqliteRepository::open_read_only(&db).ok()?;
    oak_cli::http::view_of_checkout(&repo)
}

/// `oak ls|cat|grep` only read the remote for now; say so instead of
/// silently doing a remote read the caller didn't ask for.
fn require_remote_flag(command: &str, remote: bool) -> oak_core::Result<()> {
    if remote {
        Ok(())
    } else {
        Err(oak_core::OakError::InvalidArgument(format!(
            "`oak {command}` reads the remote: pass --remote (for the working tree, use your shell's {command})"
        )))
    }
}

fn remote_arg_or_env_or_default(remote: Option<&str>) -> oak_core::Result<String> {
    Ok(remote_arg_or_env(remote)?.unwrap_or_else(|| commands::push::DEFAULT_REMOTE.to_string()))
}

/// Split a full Oak repo URL (`http://host:8080/acme/demo`, the form `oak
/// serve` prints) into its server base and `owner/name` spec. The last two
/// path segments are the spec; anything before them stays on the base, so a
/// server behind a path-prefixing proxy still works. Returns `Ok(None)` for
/// anything that isn't an http(s) URL so ordinary specs pass through.
fn split_oak_repo_url(name: &str) -> oak_core::Result<Option<(String, String)>> {
    let trimmed = name.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        || commands::git_clone::looks_like_git_url(trimmed)
    {
        return Ok(None);
    }
    let invalid = |why: &str| {
        oak_core::OakError::InvalidArgument(format!(
            "Invalid Oak repo URL: {why} (expected http(s)://HOST/ORG/REPO)"
        ))
    };
    let mut url = reqwest::Url::parse(trimmed).map_err(|_| invalid("not a valid URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(
            "credentials do not belong in the URL; use OAK_API_KEY or `oak login`",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("query strings and fragments are not allowed"));
    }
    let segments: Vec<String> = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let [prefix @ .., owner, repo] = segments.as_slice() else {
        return Err(invalid("the path must end in ORG/REPO"));
    };
    let spec = format!("{owner}/{repo}");
    commands::parse_owner_repo(&spec)?;
    url.set_path(&prefix.join("/"));
    let base = commands::push::normalize_remote_url(url.as_str())
        .ok_or_else(|| invalid("not an http(s) server URL"))?;
    Ok(Some((base, spec)))
}

fn validate_clone_integrity_override_target(
    name: Option<&str>,
    allow_unverified_integrity: bool,
    allow_legacy_scope: bool,
) -> oak_core::Result<()> {
    if (allow_unverified_integrity || allow_legacy_scope)
        && name.is_some_and(commands::git_clone::looks_like_git_url)
    {
        return Err(oak_core::OakError::InvalidArgument(
            "clone integrity overrides require an ORG/REPO Oak spec".to_string(),
        ));
    }
    Ok(())
}

fn validate_expected_clone_head(
    branch: Option<&str>,
    detached: bool,
    expected_head: Option<&str>,
) -> oak_core::Result<()> {
    let Some(expected) = expected_head else {
        return Ok(());
    };
    if branch.is_none() && !detached {
        return Err(oak_core::OakError::InvalidArgument(
            "clone --expected-head requires --branch NAME or --detached".to_string(),
        ));
    }
    if expected.len() != 64 || oak_core::Hash::from_hex(expected).is_err() {
        return Err(oak_core::OakError::InvalidArgument(
            "clone --expected-head requires a full lowercase 64-character Oak commit hash"
                .to_string(),
        ));
    }
    Ok(())
}

/// Print one help line. The `colors` constants render as nothing when stdout
/// isn't a terminal (same gate as all command output — see `output`), so the
/// line arrives here already plain or already colored.
fn help_line(s: &str) {
    output::print_line(s);
}

fn print_help() {
    use output::colors::*;

    let version = commands::update_channel::version_line();

    // Monochrome wordmark banner. The old green tree-glyph mark is retired
    // — the oak symbol is now the compass mark used on oak.space, which a
    // terminal can't render faithfully, so the banner leads with the
    // wordmark instead.
    help_line(&format!("{BOLD}Oak VCS{RESET}"));
    help_line(&format!("{DIM}v{version}{RESET}\n"));
    help_line(&format!(
        "{WHITE}{BOLD}Usage:{RESET} oak {DIM}[--verbose]{RESET} <command> {DIM}[args]{RESET}\n"
    ));

    // The baseline flow — what most people want on day one.
    help_line(&format!("{BRIGHT_GREEN}{BOLD}Getting started{RESET}"));
    for (cmd, desc) in [
        ("oak init", "Create a repository in the current directory"),
        ("oak clone ORG/REPO", "Or clone an existing one"),
        (
            "oak start \"SUMMARY\"",
            "Start a task: a branch kept merged and pushed",
        ),
        ("oak commit", "Checkpoint your work locally"),
        ("oak merge", "Squash-merge the task onto main"),
        ("oak push --repo ORG/REPO", "First push of a new repo"),
    ] {
        help_line(&format!("  {BRIGHT_GREEN}{cmd:<26}{RESET}{desc}"));
    }

    // The full command list: names and one-line descriptions only. Flags
    // live in `oak <command> --help`, so every row fits in 80 columns.
    for (label, color, commands) in help_groups() {
        help_line(&format!("\n{color}{BOLD}{label}{RESET}"));
        for (name, desc) in commands {
            help_line(&format!("  {color}{name:<HELP_NAME_WIDTH$}{RESET}{desc}"));
        }
    }

    help_line(&format!(
        "\nRun {GREEN}oak <command> --help{RESET} for a command's flags and details.\n"
    ));
    for line in HELP_FOOTER {
        help_line(line);
    }
    help_line("");
    help_line(&format!(
        "{WHITE}{BOLD}Docs{RESET}        {GREEN}{UNDERLINE}https://oak.space/docs{RESET}"
    ));
    help_line(&format!(
        "{WHITE}{BOLD}Discord{RESET}     {GREEN}{UNDERLINE}https://oak.space/discord{RESET}"
    ));
}

/// Width of the command-name column in `oak --help`. With a two-space
/// indent, a description of up to 62 characters keeps a row within 80.
const HELP_NAME_WIDTH: usize = 14;

const HELP_FOOTER: &[&str] = &[
    "Exit codes  0 ok · 1 error · 2 usage · 3 locked · 4 dirty tree · 5 conflicts",
    "            6 network/server/auth · 7 merge uncertified · 8 integrity unproven",
    "With --json, any failure is one JSON document on stdout with an \"error\" key.",
];

type HelpGroup = (
    &'static str,
    output::colors::Color,
    Vec<(&'static str, &'static str)>,
);

/// The command groups in `oak --help`. Each group is tinted by the domain it
/// acts on, so related commands read as a block:
///   GREEN    the local repository and working tree
///   YELLOW   branches and tasks (moves HEAD around, lands on main)
///   BLUE     talks to the server: sync, account, groups, CI, sites
///   CYAN     read-only inspection (history, refs, remote peeks)
///   MAGENTA  mounts & virtual filesystems
///   WHITE    storage and misc utilities
///
/// Keep this in sync with the clap `Commands` enum: every user-facing
/// top-level command should appear in exactly one group.
fn help_groups() -> Vec<HelpGroup> {
    use output::colors::*;
    let mut groups: Vec<HelpGroup> = vec![
        (
            "Repository & working tree",
            GREEN,
            vec![
                (
                    "init",
                    "Create a repository here (--git for a git-backed one)",
                ),
                ("clone", "Clone a repository, or pick one interactively"),
                ("status", "Show working tree status"),
                ("diff", "Browse changes vs HEAD, a branch, or a commit"),
                ("commit", "Checkpoint your changes locally"),
                ("restore", "Restore files from HEAD or a commit"),
                ("reset", "Discard uncommitted changes"),
                ("conflict", "Inspect or resolve in-progress conflicts"),
                ("change", "Capture uncommitted changes without committing"),
            ],
        ),
        (
            "Branches & tasks",
            YELLOW,
            vec![
                (
                    "start",
                    "Start a task: a new branch, described, pushed, watched",
                ),
                ("switch", "Switch to, create, or stack a branch"),
                ("branch", "List, inspect, review, or rename branches"),
                ("desc", "Set the branch description (the merge message)"),
                (
                    "finish",
                    "Finalize a branch: preflight, push, description sync",
                ),
                (
                    "approve",
                    "Approve a branch's changes in paths your groups own",
                ),
                ("merge", "Squash-merge a branch into main on the server"),
                ("close", "Close a branch without merging"),
                (
                    "split",
                    "Split a branch's commits into independent branches",
                ),
                ("checkout", "Detach HEAD at a commit"),
                (
                    "root",
                    "Separate timelines in this repo, like release lines",
                ),
            ],
        ),
        (
            "Sync with the server",
            BLUE,
            vec![
                ("push", "Publish the current branch"),
                ("pull", "Update this branch, then merge in its parent"),
                ("fetch", "Refresh the local copy of main"),
                ("watch", "Keep this branch merged with main and pushed"),
            ],
        ),
        (
            "History & inspection",
            CYAN,
            vec![
                ("log", "Show commit history"),
                ("hash", "Print the current HEAD commit hash"),
                ("info", "Show repository and branch metadata"),
                ("agent state", "The repo's state and next actions, as JSON"),
                ("file", "Verify one committed file's bytes"),
                ("tree", "List a commit's paths, sizes, and hashes"),
                ("refs", "Check local refs for disagreements"),
                (
                    "ls|cat|grep",
                    "Peek at the remote without widening your checkout",
                ),
                ("sparse", "Manage a partial checkout"),
            ],
        ),
        (
            "Account, groups & access",
            BLUE,
            vec![
                ("login", "Log in to an Oak server"),
                ("logout", "Log out of an Oak server"),
                ("whoami", "Show the logged-in username"),
                ("auth", "Show which credential is in use and who you are"),
                ("repo", "List, move, or change the visibility of repos"),
                ("open", "Open the repo or branch in a browser"),
                ("groups", "List the groups you're in"),
                ("group", "Manage an org's groups and their repo access"),
                ("access", "List a repo's path rules and who owns them"),
                (
                    "owners",
                    "Show who owns a path, or what this branch touches",
                ),
            ],
        ),
        (
            "CI & sites",
            BLUE,
            vec![
                ("ci", "CI runs on main, logs, and protected publishes"),
                ("site", "Oak Pages sites served at https://NAME.oak.page/"),
            ],
        ),
    ];

    // Mount is built on macOS, Linux, and Windows.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    groups.push((
        "Mounts",
        MAGENTA,
        vec![("mount", "Mount a repo as a lazy virtual filesystem")],
    ));

    groups.extend([
        (
            "Storage & health",
            WHITE,
            vec![
                (
                    "retention",
                    "Keep only the newest versions of files in a directory",
                ),
                ("doctor", "Check remote content integrity, commits to bytes"),
                (
                    "blob info",
                    "Inspect one blob's metadata, chunks, and paths",
                ),
                (
                    "maintenance",
                    "Compact the local database and reclaim space",
                ),
            ],
        ),
        (
            "More",
            WHITE,
            vec![
                ("export", "Export history into a git repository"),
                ("archive", "Create a zip archive"),
                ("environment", "List the environment variables Oak reads"),
                ("config", "Read or change agent-sync settings"),
                ("mcp", "Run an MCP server over stdio for agents"),
                ("skill", "Install the agent skill that teaches agents Oak"),
                ("feedback", "Send feedback to the Oak team"),
                ("completions", "Print a shell-completion script"),
                ("upgrade", "Upgrade the Oak CLI"),
            ],
        ),
    ]);
    groups
}

#[cfg(test)]
mod contract_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn merge_invariant_violation(detail: String) -> oak_core::OakError {
        oak_core::OakError::MergeInvariantViolation {
            detail,
            next_commands: Vec::new(),
        }
    }

    fn merge_prediction_uncertified(detail: String) -> oak_core::OakError {
        oak_core::OakError::MergePredictionUncertified {
            detail,
            next_commands: Vec::new(),
        }
    }

    fn exit_code_of(err: &oak_core::OakError) -> i32 {
        oak_cli::contract::classify(err).exit.code()
    }
    use clap::Parser;

    /// Every command's one-line summary stays short enough for a
    /// `Commands:` list row, and its `--help` wraps within 80 columns. clap
    /// takes the summary from the first paragraph of the doc comment, so a
    /// long first paragraph shows up here: give it a short summary line and
    /// move the detail to a paragraph below. Usage lines are exempt (clap
    /// doesn't wrap them).
    #[test]
    fn command_help_fits_in_80_columns() {
        use clap::CommandFactory;
        fn walk(cmd: &clap::Command, path: &str, problems: &mut Vec<String>) {
            let path = format!("{path} {}", cmd.get_name());
            if let Some(about) = cmd.get_about() {
                let about = about.to_string();
                if about.chars().count() > 60 || about.contains('\n') {
                    problems.push(format!("{path}: summary over 60 chars: {about:?}"));
                }
            }
            let help = cmd.clone().term_width(80).render_long_help().to_string();
            for line in help.lines() {
                if line.chars().count() > 80 && !line.starts_with("Usage:") {
                    problems.push(format!("{path}: help line over 80 chars: {line:?}"));
                }
            }
            for sub in cmd.get_subcommands().filter(|c| shown(c)) {
                walk(sub, &path, problems);
            }
        }
        // Hidden commands (hook entrypoints, internals) and clap's generated
        // `help` tree aren't part of the help a person reads.
        fn shown(cmd: &clap::Command) -> bool {
            !cmd.is_hide_set() && cmd.get_name() != "help"
        }
        let mut root = Cli::command();
        root.build();
        let mut problems = Vec::new();
        for sub in root.get_subcommands().filter(|c| shown(c)) {
            walk(sub, "oak", &mut problems);
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn clone_accepts_a_full_repo_url() {
        let split = |s: &str| split_oak_repo_url(s).unwrap();
        assert_eq!(
            split("http://127.0.0.1:8080/acme/demo"),
            Some(("http://127.0.0.1:8080".to_string(), "acme/demo".to_string()))
        );
        assert_eq!(
            split("https://git.example.com/oak/acme/demo/"),
            Some((
                "https://git.example.com/oak".to_string(),
                "acme/demo".to_string()
            ))
        );
        // Plain specs and git URLs are someone else's job.
        assert_eq!(split("acme/demo"), None);
        assert_eq!(split("https://github.com/acme/demo"), None);
        assert_eq!(split("https://example.com/acme/demo.git"), None);
        for bad in [
            "http://127.0.0.1:8080/demo",
            "http://127.0.0.1:8080/",
            "http://user:pw@127.0.0.1:8080/acme/demo",
            "http://127.0.0.1:8080/acme/demo?x=1",
            "http://127.0.0.1:8080/acme/..",
        ] {
            assert!(split_oak_repo_url(bad).is_err(), "{bad} should be rejected");
        }
    }

    fn parse_site(args: &[&str]) -> SiteCommands {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Site { action } => action,
            _ => panic!("expected the Site subcommand"),
        }
    }

    #[test]
    fn site_subcommands_share_repo_remote_and_json_flags() {
        let cmd = parse_site(&[
            "oak",
            "site",
            "create",
            "docs",
            "--dir",
            "public",
            "--spa",
            "--repo",
            "acme/web",
            "-r",
            "https://example.test",
            "--json",
        ]);
        let target = cmd.target().clone();
        assert_eq!(target.repo.as_deref(), Some("acme/web"));
        assert_eq!(target.remote.as_deref(), Some("https://example.test"));
        assert!(target.json);
        let (_, action, confirmed) = cmd.into_action().unwrap();
        assert!(confirmed);
        assert!(matches!(
            action,
            commands::site::SiteAction::Create { ref name, ref dir, spa: true }
                if name == "docs" && dir.as_deref() == Some("public")
        ));

        let (_, action, _) = parse_site(&["oak", "site", "list", "--org", "acme"])
            .into_action()
            .unwrap();
        assert!(
            matches!(action, commands::site::SiteAction::List { org: Some(ref o) } if o == "acme")
        );

        let (_, action, _) = parse_site(&["oak", "site", "rollback", "docs", "--to", "42"])
            .into_action()
            .unwrap();
        assert!(matches!(
            action,
            commands::site::SiteAction::Rollback { to: Some(42), .. }
        ));

        let (_, action, _) = parse_site(&["oak", "site", "rename", "docs", "handbook"])
            .into_action()
            .unwrap();
        assert!(matches!(
            action,
            commands::site::SiteAction::Rename { ref new_name, .. } if new_name == "handbook"
        ));
    }

    #[test]
    fn site_rm_needs_yes_to_be_confirmed() {
        let (_, _, confirmed) = parse_site(&["oak", "site", "rm", "docs"])
            .into_action()
            .unwrap();
        assert!(!confirmed);
        let (_, _, confirmed) = parse_site(&["oak", "site", "rm", "docs", "--yes"])
            .into_action()
            .unwrap();
        assert!(confirmed);
    }

    #[test]
    fn site_set_maps_spa_and_password_flags() {
        use commands::site::{PasswordChange, SiteAction};
        let temp = tempfile::TempDir::new().unwrap();
        let pw = temp.path().join("pw");
        std::fs::write(&pw, "hunter2\n").unwrap();
        let pw = pw.to_str().unwrap();

        let (_, action, _) = parse_site(&[
            "oak",
            "site",
            "set",
            "docs",
            "--no-spa",
            "--password-user",
            "ann",
            "--password-file",
            pw,
        ])
        .into_action()
        .unwrap();
        match action {
            SiteAction::Set {
                spa, password, dir, ..
            } => {
                assert_eq!(spa, Some(false));
                assert_eq!(dir, None);
                assert_eq!(
                    password,
                    Some(PasswordChange::Set {
                        username: "ann".into(),
                        password: "hunter2".into()
                    })
                );
            }
            _ => panic!("expected Set"),
        }

        let (_, action, _) = parse_site(&["oak", "site", "set", "docs", "--no-password"])
            .into_action()
            .unwrap();
        assert!(matches!(
            action,
            SiteAction::Set {
                password: Some(PasswordChange::Remove),
                spa: None,
                ..
            }
        ));

        for bad in [
            &["oak", "site", "set", "docs", "--spa", "--no-spa"][..],
            &["oak", "site", "set", "docs", "--password-user", "ann"],
            &["oak", "site", "set", "docs", "--password-file", "f"],
            &[
                "oak",
                "site",
                "set",
                "docs",
                "--no-password",
                "--password-user",
                "a",
                "--password-file",
                "f",
            ],
            &["oak", "site", "enable"],
        ] {
            assert!(
                Cli::try_parse_from(bad).is_err(),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn open_accepts_a_pages_site() {
        match Cli::try_parse_from(["oak", "open", "--site", "docs", "--print"])
            .unwrap()
            .command
        {
            Commands::Open { site, print, .. } => {
                assert_eq!(site.as_deref(), Some("docs"));
                assert!(print);
            }
            _ => panic!("expected Open"),
        }
    }

    /// Extract the `Clone` variant's `shallow` flag from a parsed arg vector.
    fn parse_clone_shallow(args: &[&str]) -> bool {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Clone(CloneArgs { shallow, .. }) => shallow,
            _ => panic!("expected the Clone subcommand"),
        }
    }

    fn parse_clone_branch(args: &[&str]) -> Option<String> {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Clone(CloneArgs { branch, .. }) => branch,
            _ => panic!("expected the Clone subcommand"),
        }
    }

    fn parse_clone_integrity_overrides(args: &[&str]) -> (bool, bool) {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Clone(CloneArgs {
                allow_unverified_integrity,
                allow_legacy_scope,
                ..
            }) => (allow_unverified_integrity, allow_legacy_scope),
            _ => panic!("expected the Clone subcommand"),
        }
    }

    fn parse_switch_policy(args: &[&str]) -> (bool, bool, bool) {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Switch {
                create,
                clean,
                discard,
                ..
            } => (create, clean, discard),
            _ => panic!("expected the Switch subcommand"),
        }
    }

    #[test]
    fn merge_invariant_violation_uses_conflict_exit_code() {
        // fb-105: a dry-run that predicts destroyed target-side state must
        // exit non-zero (merge-blocking family).
        assert_eq!(
            exit_code_of(&merge_invariant_violation(
                "the predicted merge result destroys target-side state".to_string(),
            )),
            5
        );
    }

    #[test]
    fn merge_prediction_uncertified_uses_distinct_exit_code() {
        // fb-105: automation must be able to tell "certified safe" (0) from
        // "unable to certify" without parsing JSON; 7 is reserved for it.
        assert_eq!(
            exit_code_of(&merge_prediction_uncertified(
                "no authoritative target head".to_string(),
            )),
            7
        );
    }

    #[test]
    fn inconclusive_integrity_proof_uses_its_own_exit_code() {
        // An exhausted proof is neither success nor a server/content failure.
        assert_eq!(
            exit_code_of(&oak_core::OakError::IntegrityInconclusive {
                detail: "wall budget".to_string(),
                recommended_next_commands: Vec::new(),
            }),
            8
        );
        // A refused admission is a transient server-side condition.
        assert_eq!(
            exit_code_of(&oak_core::OakError::IntegrityAdmissionBusy(
                "busy".to_string(),
            )),
            6
        );
    }

    #[test]
    fn merge_base_unavailable_errors_use_conflict_exit_code() {
        assert_eq!(
            exit_code_of(&oak_core::OakError::NoVerifiedCommonAncestor {
                left: "topic".to_string(),
                right: "main".to_string(),
            }),
            5
        );
        assert_eq!(
            exit_code_of(&oak_core::OakError::IncompleteManifestData {
                left: "topic".to_string(),
                right: "main".to_string(),
                missing: "abc123".to_string(),
            }),
            5
        );
        assert_eq!(
            exit_code_of(&oak_core::OakError::IncompleteCommitData {
                context: "effective HEAD for branch 'topic'".to_string(),
                missing: "def456".to_string(),
            }),
            5
        );
    }

    #[test]
    fn clone_is_full_by_default() {
        // The default: `oak clone <repo>` downloads the full history, so
        // `shallow` is false unless the user opts in.
        assert!(!parse_clone_shallow(&["oak", "clone", "oak/oak"]));
        assert!(!parse_clone_shallow(&["oak", "clone"]));
    }

    #[test]
    fn clone_shallow_flag_opts_into_shallow_history() {
        assert!(parse_clone_shallow(&[
            "oak",
            "clone",
            "oak/oak",
            "--shallow"
        ]));
        // Order-independent and composes with other flags.
        assert!(parse_clone_shallow(&[
            "oak",
            "clone",
            "--shallow",
            "oak/oak"
        ]));
        assert!(parse_clone_shallow(&[
            "oak",
            "clone",
            "oak/oak",
            "dest",
            "--shallow"
        ]));
    }

    #[test]
    fn clone_full_flag_is_an_accepted_no_op() {
        // `--full` is retained as a hidden no-op (full is now the default), so
        // old invocations still parse and stay non-shallow.
        assert!(!parse_clone_shallow(&["oak", "clone", "oak/oak", "--full"]));
    }

    #[test]
    fn clone_integrity_overrides_are_explicit_and_independent() {
        assert_eq!(
            parse_clone_integrity_overrides(&["oak", "clone", "oak/oak"]),
            (false, false)
        );
        assert_eq!(
            parse_clone_integrity_overrides(&[
                "oak",
                "clone",
                "oak/oak",
                "--allow-unverified-integrity",
                "--allow-legacy-scope",
            ]),
            (true, true)
        );
    }

    #[test]
    fn interactive_clone_accepts_integrity_overrides_for_the_selected_oak_repo() {
        validate_clone_integrity_override_target(None, true, true)
            .expect("the picker selects an Oak repo before clone preflight");
    }

    #[test]
    fn clone_expected_head_is_a_full_hash_bound_to_a_branch() {
        let full = "a".repeat(64);
        validate_expected_clone_head(Some("feature"), false, Some(&full)).unwrap();
        validate_expected_clone_head(None, true, Some(&full)).unwrap();
        validate_expected_clone_head(None, false, None).unwrap();
        assert!(validate_expected_clone_head(None, false, Some(&full)).is_err());
        assert!(validate_expected_clone_head(Some("feature"), false, Some("abc")).is_err());
        assert!(validate_expected_clone_head(None, true, Some("abc")).is_err());
        assert!(
            validate_expected_clone_head(Some("feature"), false, Some(&"A".repeat(64))).is_err()
        );
        assert!(validate_expected_clone_head(None, true, Some(&"A".repeat(64))).is_err());
    }

    #[test]
    fn clone_detached_pin_parses_but_never_combines_with_branch() {
        let full = "a".repeat(64);
        let ok = Cli::try_parse_from([
            "oak",
            "clone",
            "oak/oak",
            "--detached",
            "--expected-head",
            &full,
        ]);
        assert!(ok.is_ok(), "{:?}", ok.err());
        for argv in [
            vec!["oak", "clone", "oak/oak", "--detached", "--branch", "x"],
            vec![
                "oak",
                "clone",
                "oak/oak",
                "--detached",
                "--branch",
                "x",
                "--expected-head",
                &full,
            ],
        ] {
            assert!(Cli::try_parse_from(argv).is_err());
        }
    }

    #[test]
    fn doctor_remote_json_syntax_is_stable() {
        match Cli::try_parse_from([
            "oak",
            "doctor",
            "--repo",
            "oak/oakspace",
            "--remote",
            "https://oak.space",
            "--json",
        ])
        .expect("doctor syntax should parse")
        .command
        {
            Commands::Doctor {
                repo, remote, json, ..
            } => {
                assert_eq!(repo, "oak/oakspace");
                assert_eq!(remote.as_deref(), Some("https://oak.space"));
                assert!(json);
            }
            _ => panic!("expected Doctor"),
        }
    }

    #[test]
    fn doctor_rejects_zero_depth_during_argument_parsing() {
        let result =
            Cli::try_parse_from(["oak", "doctor", "--repo", "oak/oakspace", "--depth", "0"]);
        let error = match result {
            Ok(_) => panic!("zero depth must not reach the network layer"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("depth"));
    }

    #[test]
    fn blob_info_rejects_depth_beyond_server_bound() {
        let result = Cli::try_parse_from([
            "oak",
            "blob",
            "info",
            "abc123",
            "--repo",
            "oak/oakspace",
            "--depth",
            "10001",
        ]);
        let error = match result {
            Ok(_) => panic!("oversized depth must not reach the network layer"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("10000"));
    }

    #[test]
    fn privileged_blob_info_syntax_is_stable() {
        match Cli::try_parse_from([
            "oak",
            "blob",
            "info",
            "abc123",
            "--repo",
            "oak/oakspace",
            "--json",
        ])
        .expect("blob info syntax should parse")
        .command
        {
            Commands::Blob {
                action:
                    BlobCommands::Info {
                        hash, repo, json, ..
                    },
            } => {
                assert_eq!(hash, "abc123");
                assert_eq!(repo, "oak/oakspace");
                assert!(json);
            }
            _ => panic!("expected Blob::Info"),
        }
    }

    #[test]
    fn clone_branch_flag_parses_named_remote_branch() {
        assert_eq!(
            parse_clone_branch(&["oak", "clone", "--branch", "web-fix", "oak/oak"]).as_deref(),
            Some("web-fix")
        );
        assert_eq!(
            parse_clone_branch(&["oak", "clone", "oak/oak", "oak-copy", "--branch", "web-fix"])
                .as_deref(),
            Some("web-fix")
        );
    }

    #[test]
    fn switch_clean_flag_parses_for_create_and_existing_branch() {
        assert_eq!(
            parse_switch_policy(&["oak", "switch", "-c", "--clean"]),
            (true, true, false)
        );
        assert_eq!(
            parse_switch_policy(&["oak", "switch", "feature", "--clean"]),
            (false, true, false)
        );
    }

    fn parse_close(args: &[&str]) -> (Vec<String>, bool, Option<String>, bool) {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
            .resolve_aliases()
        {
            Commands::Close {
                names,
                remote,
                reason,
                json,
            } => (names, remote, reason, json),
            _ => panic!("expected the Close subcommand"),
        }
    }

    #[test]
    fn branch_close_resolves_to_close() {
        let args = ["--remote", "a", "b@x", "--reason", "superseded", "--json"];
        let close = parse_close(&[&["oak", "close"], &args[..]].concat());
        assert_eq!(
            close,
            (
                vec!["a".to_string(), "b@x".to_string()],
                true,
                Some("superseded".to_string()),
                true
            )
        );
        assert_eq!(
            parse_close(&[&["oak", "branch", "close"], &args[..]].concat()),
            close
        );
        // `oak branch --json close` is `--json` too, and no name means the
        // current branch.
        assert_eq!(
            parse_close(&["oak", "branch", "--json", "close"]),
            (vec![], false, None, true)
        );
    }

    #[test]
    fn branch_show_current_close_is_not_resolved() {
        let command = Cli::try_parse_from(["oak", "branch", "--show-current", "close"])
            .expect("args should parse")
            .command
            .resolve_aliases();
        assert!(matches!(
            command,
            Commands::Branch {
                show_current: true,
                action: Some(BranchCommands::Close { .. }),
                ..
            }
        ));
    }

    #[test]
    fn switch_discard_hidden_alias_still_parses_for_compatibility() {
        assert_eq!(
            parse_switch_policy(&["oak", "switch", "-c", "--discard"]),
            (true, false, true)
        );
    }
}
