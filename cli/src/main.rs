use oak_cli::commands;
use oak_cli::output;

use std::io::Read;
use std::net::IpAddr;
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
  4 dirty working tree blocked the operation
  5 merge/sync conflicts or in-progress conflict state
  6 network, server, or auth failure
  7 merge prediction uncertified (not certified safe — no authoritative target
    head, or no prediction ran at all; run `oak fetch`)

Partial clones:
  oak clone <org>/<repo> --path <prefix>   sparse (partial) clone, Perforce-style
  oak sparse                               manage the checked-out cone
  OAK_ALLOW_PARTIAL_CLONE=1                 recovery: skip blobs a broken server
                                           failed to ship (instead of erroring)";

#[derive(Parser)]
#[command(name = "oak")]
#[command(about = "Oak — Branch freely")]
#[command(version)]
#[command(styles = help_styles())]
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
    /// Initialize a new local repository in the current or specified directory
    Init {
        /// Directory to initialize (defaults to current directory)
        path: Option<PathBuf>,
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

        /// Refused compatibility flag: Oak commits are messageless; use `oak desc --file`.
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

    /// Merge a branch into its parent
    Merge {
        /// Continue a merge after resolving conflicts
        #[arg(long)]
        r#continue: bool,

        /// Abort a merge in progress
        #[arg(long)]
        abort: bool,

        /// Preview the merge locally without pushing, fetching, or changing files
        #[arg(long)]
        dry_run: bool,

        /// Branch to merge (defaults to current branch)
        branch: Option<String>,

        /// Emit machine-readable JSON. With --dry-run this emits the merge
        /// prediction; otherwise it emits the actual server merge result.
        #[arg(long)]
        json: bool,

        /// Bypass the server-side CI merge gate. Use only after inspecting
        /// the failed or stuck CI run; this maps to the merge API's
        /// `?force=1` override.
        #[arg(long)]
        force: bool,

        /// Merges onto main are CI-gated; when CI for the branch head is
        /// still running, wait for it to conclude and then merge (polling
        /// every ~20s, with a progress line per poll). Takes an optional
        /// timeout in minutes; a bare `--wait` waits up to 30 minutes. If
        /// CI concludes failure the merge errors, naming the `oak ci
        /// logs`/`oak ci rerun` follow-ups.
        #[arg(
            long,
            value_name = "MINUTES",
            num_args = 0..=1,
            default_missing_value = "30"
        )]
        wait: Option<u64>,
    },

    /// Switch to a branch or detach HEAD at a commit. With no name, pick a
    /// branch interactively.
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
    },

    /// Close a branch, defaulting to the current branch
    Close {
        /// Branch name
        name: Option<String>,

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
    },

    /// Finalize a branch with preflight, push, and description sync.
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

    /// Detach HEAD at a specific commit (full hash or unique prefix)
    Checkout {
        /// Commit hash, or a unique short prefix (≥ 4 hex chars)
        reference: String,
    },

    /// Restore working directory files to their HEAD state
    Restore {
        /// Paths to restore (restores everything if omitted)
        paths: Vec<PathBuf>,

        /// Source commit hash to restore from (defaults to HEAD)
        #[arg(short, long)]
        source: Option<String>,

        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Browse changes between the working directory, branches, or commits.
    /// On a terminal this opens the full-screen browser: arrow keys, vi/less
    /// keys (j/k, Space, f/b, d/u, g/G) and emacs keys (^N/^P, ^V/M-v, M-</M->)
    /// all navigate; `?` lists every binding
    Diff {
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

        /// Print the diff to stdout instead of opening the interactive browser
        #[arg(long, conflicts_with = "name_only")]
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
        #[arg(long, conflicts_with = "json")]
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
    },

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
        #[arg(long, conflicts_with_all = ["json", "porcelain", "short"])]
        reconcile: bool,
    },

    /// Show repository and branch metadata
    Info {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Print the current HEAD commit hash (bare, for scripting/piping)
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

    /// Show commit history. On a terminal this opens the full-screen viewer:
    /// arrow keys, vi/less keys (j/k, Space, f/b, d/u, g/G) and emacs keys
    /// (^N/^P, ^V/M-v, M-</M->, ^S search, ^G cancel) all navigate; `?` lists
    /// every binding
    Log {
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
    },

    /// Run a minimal self-hosted Oak server backed by SQLite.
    ///
    /// Serves the push/pull/clone protocol from a local data directory (one
    /// SQLite file per repo) with no organizations and no auth model. Point a
    /// CLI at it with `oak clone http://<host>:<port>/<owner>/<name>`.
    Serve {
        /// Directory to store repositories in (one `<owner>/<name>.oakdb` each)
        #[arg(short, long, default_value = "./oak-data")]
        dir: PathBuf,

        /// Port to listen on
        #[arg(short, long, default_value_t = 8080)]
        port: u16,

        /// Host/IP address to bind. Defaults to loopback; binding a non-loopback
        /// address requires --token.
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,

        /// Optional shared bearer token required on every request. When unset,
        /// the server is open only on loopback.
        #[arg(long, env = "OAK_SERVE_TOKEN")]
        token: Option<String>,
    },

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
    },

    /// Bring the local clone fully up to date: fetch new commits on the
    /// current branch from the remote, then merge in any changes from the
    /// parent branch (what `oak sync` used to do).
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

        /// Abort a parent-sync in progress
        #[arg(long)]
        abort: bool,
    },

    /// Refresh local copy of `main` from the remote without touching the
    /// working tree or running a merge. Useful for previewing what's new on
    /// main before deciding to `oak pull` or `oak merge`.
    Fetch {
        /// Remote server URL. If omitted, OAK_REMOTE overrides the stored remote for this invocation.
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Reset working directory to HEAD (discard uncommitted changes)
    Reset {
        /// Specific path to reset (file or directory). If omitted, resets entire working directory.
        path: Option<PathBuf>,

        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Clone a repository from the server. With no repository argument,
    /// opens an interactive repo picker.
    Clone {
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

        /// Clone only the newest commit on the selected branch (the default
        /// branch when --branch is omitted), like `git clone --depth=1`.
        /// This narrows local history plus preflight/recovery scope; the
        /// selected commit's working tree remains complete. By default `oak
        /// clone` downloads the full reachable history.
        #[arg(long)]
        shallow: bool,

        /// Proceed when the server's bounded metadata preflight exhausts only
        /// its history/tree/path budget. Pull still hash-verifies downloaded
        /// objects; corruption findings are never overridden.
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
    },

    /// Diagnose remote repository content from reachable commits to bytes
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

    /// Inspect privileged blob integrity evidence
    Blob {
        #[command(subcommand)]
        action: BlobCommands,
    },

    /// Static-site (Pages-style) management
    Site {
        #[command(subcommand)]
        action: SiteCommands,
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

    /// Create a zip archive of the current directory
    Archive {
        /// Output file path (defaults to <directory_name>.zip)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Inspect and control server-side CI runs for this repo.
    ///
    /// Merges onto main are CI-gated: the server refuses `oak merge` (HTTP
    /// 412) while CI for the branch head is running or after it failed.
    /// These subcommands are the visibility and recovery surface for that
    /// gate: list runs, check the gate for the current branch head, read
    /// step logs, and re-dispatch a run that failed for infra reasons.
    Ci {
        #[command(subcommand)]
        action: CiCommands,
    },

    /// Token-scoped fetch of a single commit's tree (used by native CI).
    ///
    /// Downloads the working tree at the given commit from a CI fetch endpoint
    /// and unpacks it into the destination directory. Authenticated by a
    /// short-lived, repo-scoped token (no `oak login` needed). Run inside the
    /// CI execution sandbox; not part of the normal user workflow.
    #[command(name = "ci-fetch", hide = true)]
    CiFetch {
        /// Server base URL (e.g. https://oak.space).
        #[arg(long)]
        remote: String,
        /// Commit hash to materialize.
        #[arg(long)]
        commit: String,
        /// Repo-scoped fetch token minted by the CI control plane.
        #[arg(long)]
        token: String,
        /// Destination directory (created if missing; defaults to ".").
        dest: Option<PathBuf>,
    },

    /// Export this oak repository's history into a fresh git repository.
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
        #[arg(short, long)]
        force: bool,
    },

    /// Upgrade oak to the latest version (from GitHub Releases)
    Upgrade {
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
        /// Track the canary channel (rolling pre-release built from the tip of
        /// the oak branch) instead of the latest stable release
        #[arg(long)]
        canary: bool,
    },

    /// Manage GitHub-style releases on the server (notes + downloadable artifacts)
    Release {
        #[command(subcommand)]
        action: ReleaseCommands,
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

    /// Print the logged-in username for an Oak server
    Whoami {
        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Open the project in the web browser
    Open,

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
        /// Admin-only link/unlink; omit to submit feedback as before.
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

    /// Split a branch's commits into independent branches (and reorder/drop).
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
    /// Branches are flat in Oak, so each split segment must stand alone on
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
    },

    /// Mount remote repositories as virtual filesystems, in the background.
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
    /// the `fusermount3` helper from the `fuse3` package.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
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

        /// Remote server URL.
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Manage Oak spaces: directories where an agent works across one org's
    /// repos, mounting whichever repos a task needs. Each task is a subdirectory
    /// holding one `oak mount` per repo it touches, each on its own virtual
    /// branch — like git worktrees, but without a full clone behind each one
    /// (mounts hydrate content lazily) and spanning a whole org.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    Space {
        #[command(subcommand)]
        action: SpaceCommands,
    },

    /// Manage the bundled agent skill: Oak ships a `SKILL.md` (plus reference
    /// files) in the open Agent Skills format that teaches coding agents like
    /// Claude Code how to drive Oak. The files are baked into this binary, so
    /// the installed skill always documents the CLI version that wrote it.
    Skill {
        #[command(subcommand)]
        action: SkillCommands,
    },
}

#[derive(Subcommand)]
enum BlobCommands {
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

        /// Byte budget across all --hunks patches
        #[arg(long, requires = "hunks")]
        max_bytes: Option<usize>,

        /// Number of context lines around hunks
        #[arg(short = 'U', long = "unified", value_name = "N", requires = "hunks")]
        unified: Option<usize>,

        /// Treat oversized text files as text in --json --hunks output.
        /// NUL-containing binary files still stay binary.
        #[arg(short = 'a', long = "text", requires = "hunks")]
        text: bool,

        /// Review the configured remote branch without switching to it.
        /// This refreshes local remote metadata and blobs for evidence.
        #[arg(long)]
        remote: bool,

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

        /// Review the configured remote branch without switching to it.
        /// This refreshes local remote metadata and blobs for evidence.
        #[arg(long)]
        remote: bool,
    },

    /// Batch branch triage over many branches without switching checkout
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
enum AgentCommands {
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
    },
}

#[derive(Subcommand)]
enum CiCommands {
    /// List recent CI runs for this repo (newest first).
    Runs {
        /// Maximum number of runs to show.
        #[arg(long, default_value_t = oak_cli::commands::ci::DEFAULT_RUNS_LIMIT)]
        limit: usize,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// CI state for the current branch head — the commit the merge gate
    /// checks. Exit codes: 0 CI concluded success; 1 concluded failure (or
    /// no runs found); 3 still running (retry later) — distinct so scripts
    /// can branch without parsing output.
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Wait for exact CI run ids to reach terminal states without mutating
    /// repository or CI state.
    Wait {
        /// Run ids to wait for (from `oak ci runs`, `status`, or `rerun`).
        #[arg(required = true, num_args = 1..)]
        run_ids: Vec<u64>,

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
    },

    /// Show a run's step-by-step logs.
    Logs {
        /// Run id (from `oak ci runs` / `oak ci status`).
        run_id: u64,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Re-dispatch a run's workflow at the same branch/commit (event
    /// `manual`) — for runs that failed for infra reasons rather than code.
    /// Prints the new run's id.
    Rerun {
        /// Run id to re-run (from `oak ci runs` / `oak ci status`).
        run_id: u64,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum FeedbackCommands {
    /// Link a feedback item to the branch (or commit) that addresses it.
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

    /// Remove the link between a feedback item and a branch or commit.
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

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[derive(Subcommand)]
enum SpaceCommands {
    /// Scaffold a new Oak space for an org. Creates the space directory
    /// (default `./<org>`) with an `AGENTS.md` explaining how to run tasks
    /// across the org's repos, a `CLAUDE.md` pointer, a `.claude/settings.json`,
    /// and a `.oak-space` marker recording the org.
    New {
        /// Org slug (e.g. `oak`). A legacy `org/repo` spec is accepted too —
        /// only the org segment is used.
        #[arg(value_name = "ORG")]
        spec: String,

        /// Directory to create the space in. Defaults to `./<org>`.
        dest: Option<PathBuf>,

        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// List the repos in the space's org so you can pick which to mount for a
    /// task. With no ORG, reads the `.oak-space` marker from the current
    /// directory (or an ancestor).
    Repos {
        /// Org slug to list. Defaults to the current space's org.
        #[arg(value_name = "ORG")]
        org: Option<String>,

        /// Remote server URL
        #[arg(short, long)]
        remote: Option<String>,
    },

    /// Tear down finished mounts in a space — those whose working tree is
    /// clean (committed + pushed). Dirty mounts are left alone unless
    /// `--force` is given. Operates on the current directory by default.
    Clean {
        /// Space directory to clean. Defaults to the current directory.
        dest: Option<PathBuf>,

        /// Also tear down mounts with uncommitted changes, discarding them.
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    /// Install the `oak-vcs` agent skill into the current repo's
    /// `.claude/skills/` (project-level — commit it so every collaborator's
    /// agent picks it up), or with `--global` into `~/.claude/skills/` for
    /// every project on this machine. Re-running refreshes stale files after
    /// an `oak upgrade`; up-to-date files are left untouched.
    Install {
        /// Install user-wide (~/.claude/skills) instead of into the current repo.
        #[arg(long)]
        global: bool,
    },
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[derive(Subcommand)]
enum MountCommands {
    /// List active mounts
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Safely finalize a mounted-agent task: set description, commit, push, then end.
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

    /// Unmount, drop local state, and remove the mount directory. With no
    /// DEST, tears down every mount under ~/oaktree.
    End {
        /// Mount destination directory. Omit to end every mount under ~/oaktree.
        dest: Option<PathBuf>,

        /// Discard any uncommitted overlay changes. Without this, `end`
        /// refuses to operate on a mount with dirty files so you don't
        /// silently lose work.
        #[arg(short, long)]
        force: bool,
    },

    /// Remove a mount's registry entry without unmounting or touching its
    /// on-disk state. For stale registrations left behind by a crash or
    /// reboot; refuses a live mount unless --force is given.
    Forget {
        /// Mount destination directory.
        dest: PathBuf,

        /// Drop the registration even if the mount looks live.
        #[arg(short, long)]
        force: bool,
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
    /// read from the hook's stdin JSON, then print the path. Wired into an
    /// Oak space's `.claude/settings.json`; not meant to be run by hand.
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

#[derive(Subcommand)]
enum SiteCommands {
    /// Enable Pages-style hosting for an organization, serving the chosen
    /// repo's `main` branch at <organization>.<base_domain>/. Defaults:
    /// source=/. From inside a checkout, the repo and organization are
    /// inferred from the cwd; pass --repo org/repo to override.
    Enable {
        /// Repo to publish (org/repo). Defaults to the repo for the
        /// current directory. The org is the organization whose site is
        /// being configured.
        #[arg(short, long, value_name = "ORG/REPO")]
        repo: Option<String>,
        /// Remote server URL
        #[arg(long)]
        remote: Option<String>,
        /// Branch whose head is served (default: main; currently always main)
        #[arg(short, long)]
        branch: Option<String>,
        /// Repo-relative source directory (default: /)
        #[arg(short, long)]
        source: Option<String>,
    },

    /// Disable Pages hosting for an organization.
    Disable {
        /// Organization slug to operate on. Defaults to the organization owning
        /// the current directory's repo.
        #[arg(short, long)]
        organization: Option<String>,
        /// Remote server URL
        #[arg(long)]
        remote: Option<String>,
    },

    /// Show the site config for an organization.
    Show {
        /// Organization slug to operate on. Defaults to the organization owning
        /// the current directory's repo.
        #[arg(short, long)]
        organization: Option<String>,
        /// Remote server URL
        #[arg(long)]
        remote: Option<String>,
    },

    /// List all organization sites the caller can see.
    List {
        /// Remote server URL
        #[arg(long)]
        remote: Option<String>,
    },
}

#[derive(Subcommand)]
enum SparseCommands {
    /// Show the active cone (default when no subcommand is given).
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Replace the cone with exactly these path prefixes, then re-sync the
    /// working tree. Repeatable or comma-separated.
    Set {
        /// Path prefixes to scope the checkout to (e.g. `src docs/api`).
        #[arg(value_name = "PREFIX", required = true)]
        paths: Vec<String>,
    },

    /// Add path prefixes to the existing cone, then re-sync the working tree.
    Add {
        /// Path prefixes to include (e.g. `libs/shared`).
        #[arg(value_name = "PREFIX", required = true)]
        paths: Vec<String>,
    },

    /// Drop the cone and return to a full checkout. Files whose content was
    /// never downloaded are reported — run `oak pull` to hydrate them.
    Disable,
}

#[derive(Subcommand)]
enum MaintenanceCommands {
    /// Compact the local database: convert to the compact tree/blob storage
    /// format, drop the legacy tree_entries table, and VACUUM to reclaim space.
    Compact,
}

#[derive(Subcommand)]
enum ReleaseCommands {
    /// Cut a new release
    New {
        /// Tag name (e.g. v1.0.0). Free-form — doesn't have to match an oak tag.
        tag: String,

        /// Human-friendly title (defaults to the tag name)
        #[arg(short, long)]
        title: Option<String>,

        /// Markdown release notes
        #[arg(short, long)]
        notes: Option<String>,

        /// Source commit hash (defaults to whatever's already implied; optional)
        #[arg(short, long)]
        commit: Option<String>,

        /// Create as a draft (hidden from anonymous viewers; not yet published)
        #[arg(long)]
        draft: bool,

        /// Mark as a pre-release (excluded from "latest stable" lookups)
        #[arg(long)]
        prerelease: bool,
    },

    /// List releases for this repo
    List,

    /// Show release details
    Show {
        /// Tag name
        tag: String,
    },

    /// Edit a release's title / notes / state
    Edit {
        /// Tag name
        tag: String,

        /// New title
        #[arg(short, long)]
        title: Option<String>,

        /// New notes (markdown)
        #[arg(short, long)]
        notes: Option<String>,

        /// Flip the draft flag
        #[arg(long)]
        draft: Option<bool>,

        /// Flip the prerelease flag
        #[arg(long)]
        prerelease: Option<bool>,
    },

    /// Publish a draft release
    Publish {
        /// Tag name
        tag: String,
    },

    /// Upload one or more artifacts to a release
    Upload {
        /// Tag name
        tag: String,

        /// One or more files to attach
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },

    /// Remove an asset from a release
    DeleteAsset {
        /// Tag name
        tag: String,

        /// Asset filename
        filename: String,
    },

    /// Delete a release entirely
    Delete {
        /// Tag name
        tag: String,
    },
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

/// Gate a CLI subcommand on the feature-flag system. Returns `Err` with a
/// user-facing message when the feature isn't unlocked, so `main` can print
/// it via `output::error` and exit nonzero.
///
/// The mechanism is the same as the web product (see
/// [`oak_core::features`]) — but where the web UI keys off the signed-in
/// user's admin status, the CLI keys off the `OAK_FEATURES` env var. See
/// `feature_enabled_in_cli` for the parse rules.
fn require_cli_feature(feature: oak_core::features::Feature) -> Result<(), String> {
    if oak_core::features::feature_enabled_in_cli(feature) {
        Ok(())
    } else {
        Err(format!(
            "The `{slug}` feature is gated. Set {env}={slug} (or {env}=1 \
             to enable all gated features) to use this command.",
            slug = feature.slug(),
            env = oak_core::features::CLI_FEATURES_ENV,
        ))
    }
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

fn command_outputs_structured(command: &Commands) -> bool {
    match command {
        Commands::Status {
            json,
            compact,
            porcelain,
            short,
            reconcile: _,
        } => *json || *compact || *porcelain || *short,
        Commands::Log { json, .. } | Commands::Info { json } | Commands::Diff { json, .. } => *json,
        Commands::Close { json, .. } => *json,
        Commands::Merge { json, .. } => *json,
        Commands::Commit { json, .. } | Commands::Finish { json, .. } => *json,
        Commands::Push { json, .. } => *json,
        Commands::Feedback { json, action, .. } => {
            *json
                || matches!(
                    action,
                    Some(
                        FeedbackCommands::Link { json: true, .. }
                            | FeedbackCommands::Unlink { json: true, .. }
                    )
                )
        }
        Commands::Doctor { json, .. }
        | Commands::Blob {
            action: BlobCommands::Info { json, .. },
        } => *json,
        Commands::Ci { action } => matches!(
            action,
            CiCommands::Runs { json: true, .. }
                | CiCommands::Status { json: true }
                | CiCommands::Wait { json: true, .. }
                | CiCommands::Logs { json: true, .. }
                | CiCommands::Rerun { json: true, .. }
        ),
        Commands::Agent {
            action: AgentCommands::State { json, .. },
        } => *json,
        Commands::Conflict { action } => matches!(
            action,
            ConflictCommands::Status { json: true }
                | ConflictCommands::Show { json: true }
                | ConflictCommands::Take { json: true, .. }
        ),
        Commands::Branch { json, action, .. } => {
            *json
                || matches!(
                    action,
                    Some(
                        BranchCommands::List { json: true, .. }
                            | BranchCommands::Show { json: true, .. }
                            | BranchCommands::Diff { json: true, .. }
                            | BranchCommands::Review { json: true, .. }
                            | BranchCommands::Triage { json: true, .. }
                    )
                )
        }
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        Commands::Mount {
            action:
                Some(MountCommands::List { json: true } | MountCommands::Finish { json: true, .. }),
            ..
        } => true,
        _ => false,
    }
}

fn command_outputs_json_error_envelope(command: &Commands) -> bool {
    match command {
        Commands::Status { json: true, .. }
        | Commands::Log { json: true, .. }
        | Commands::Info { json: true }
        | Commands::Diff { json: true, .. }
        | Commands::Close { json: true, .. }
        | Commands::Commit { json: true, .. }
        | Commands::Finish { json: true, .. }
        | Commands::Push { json: true, .. }
        | Commands::Doctor { json: true, .. }
        | Commands::Blob {
            action: BlobCommands::Info { json: true, .. },
        } => true,
        // Dry-run included: a dry-run that fails BEFORE printing anything
        // (not a repository, an unresolvable branch, a rejected flag combo)
        // must still emit an error envelope, or `--json` silently produces
        // zero bytes on stdout. Suppression for the post-payload verdict
        // errors is handled at the error site by
        // `output::json_payload_emitted()`, which is only true once the
        // dry-run has actually written its JSON document — a payload write
        // that failed leaves it false, so the envelope still happens.
        Commands::Merge { json: true, .. } => true,
        Commands::Ci { action } => matches!(
            action,
            CiCommands::Runs { json: true, .. }
                | CiCommands::Status { json: true }
                | CiCommands::Wait { json: true, .. }
                | CiCommands::Logs { json: true, .. }
                | CiCommands::Rerun { json: true, .. }
        ),
        Commands::Agent {
            action: AgentCommands::State { json: true, .. },
        } => true,
        Commands::Conflict {
            action:
                ConflictCommands::Status { json: true }
                | ConflictCommands::Show { json: true }
                | ConflictCommands::Take { json: true, .. },
        } => true,
        Commands::Branch { json, action, .. } => {
            *json
                || matches!(
                    action,
                    Some(
                        BranchCommands::List { json: true, .. }
                            | BranchCommands::Show { json: true, .. }
                            | BranchCommands::Diff { json: true, .. }
                            | BranchCommands::Review { json: true, .. }
                            | BranchCommands::Triage { json: true, .. }
                    )
                )
        }
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        Commands::Mount {
            action:
                Some(MountCommands::List { json: true } | MountCommands::Finish { json: true, .. }),
            ..
        } => true,
        _ => false,
    }
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

fn commit_positional_looks_like_message(cwd: &std::path::Path, paths: &[PathBuf]) -> bool {
    if positionals_after_double_dash().is_some() || paths.len() != 1 {
        return false;
    }
    let arg = paths[0].as_os_str().to_string_lossy();
    !cwd.join(&paths[0]).exists() && arg.chars().any(char::is_whitespace)
}

/// Stable Oak CLI exit-code contract:
/// 0 success; 1 generic error; 2 usage error; 3 repository locked/retryable;
/// 4 dirty working tree blocked the operation; 5 merge/sync conflicts or an
/// in-progress conflict state; 6 network/server/auth failure; 7 merge
/// prediction uncertified — NOT certified safe, whether because no
/// authoritative target head backed the classification or because no
/// prediction ran at all (fetch and retry). Only 0 means "certified safe".
fn exit_code(err: &oak_core::OakError) -> i32 {
    use oak_core::OakError;

    match err {
        OakError::InvalidArgument(_) | OakError::InvalidPath(_) => 2,
        OakError::RepoLocked => 3,
        OakError::DirtyWorkingTree(_) | OakError::UncommittedChanges => 4,
        OakError::ConflictDetected
        | OakError::LocalCommitsNotInRemoteHistory
        | OakError::RemoteCommitsNotInLocalHistory
        | OakError::IncompleteAncestry { .. }
        | OakError::IncompleteManifestData { .. }
        | OakError::IncompleteCommitData { .. }
        | OakError::IncompleteBlobData { .. }
        | OakError::NoVerifiedCommonAncestor { .. }
        | OakError::MergeConflict(_)
        | OakError::MergeInvariantViolation(_)
        | OakError::MergeInProgress => 5,
        OakError::MergePredictionUncertified(_) => 7,
        OakError::Http(_)
        | OakError::Server(_)
        | OakError::R2(_)
        | OakError::RemoteRepoNotFound(_)
        | OakError::RemoteRepoAlreadyExists(_)
        | OakError::FinishPhaseFailed(_)
        | OakError::CommitPhaseFailed(_) => 6,
        OakError::FinishPreflight(details) if details.blocker == "invalid_description" => 2,
        OakError::FinishPreflight(details)
            if matches!(
                details.blocker.as_str(),
                "auth_missing" | "auth_failed" | "remote_unreachable" | "remote_repo_missing"
            ) =>
        {
            6
        }
        OakError::FinishPreflight(_) => 1,
        _ => 1,
    }
}

fn read_desc_input(
    command: &str,
    description: Option<String>,
    file: Option<String>,
) -> oak_core::Result<String> {
    match (description, file) {
        (Some(description), None) => Ok(description),
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
    if args.len() <= 1 || (args.len() == 2 && (args[1] == "--help" || args[1] == "-h")) {
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
    // Usage errors keep clap's stderr rendering and exit code 2.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => match err.kind() {
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                output::print_raw(&output::clap_message_text(
                    &err.render(),
                    output::stdout_colors_enabled(),
                ));
                output::exit_process(0);
            }
            _ => err.exit(),
        },
    };

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

    let cwd = std::env::current_dir().expect("Failed to get current directory");

    // Create a single tokio runtime for all async commands
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // `oak upgrade` already does its own version check; skip the daily notice
    // for it to avoid a redundant prompt.
    let skip_version_notice = matches!(
        cli.command,
        Commands::Upgrade { .. } | Commands::Completions { .. }
    );
    let skip_version_notice = skip_version_notice || command_outputs_structured(&cli.command);
    let json_error_envelope = command_outputs_json_error_envelope(&cli.command);
    output::configure_activity(command_outputs_structured(&cli.command));
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

    let result = match cli.command {
        Commands::Init { path } => {
            let target = path
                .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
                .unwrap_or_else(|| cwd.clone());
            commands::init::run(&target, std::io::IsTerminal::is_terminal(&std::io::stdin()))
        }

        Commands::Commit {
            paths,
            message,
            no_verify,
            push,
            json,
            quiet,
        } => {
            if message.is_some() {
                Err(oak_core::OakError::InvalidArgument(
                    "oak commit does not take a message. Oak commits are messageless; write the branch narrative with `oak desc --file <file>` before finishing."
                        .to_string(),
                ))
            } else if commit_positional_looks_like_message(&cwd, &paths) {
                Err(oak_core::OakError::InvalidArgument(
                    "oak commit takes path filters, not a message. Oak commits are messageless; write the branch narrative with `oak desc --file <file>` before finishing. Use `oak commit -- <path>` for a deleted path that no longer exists."
                        .to_string(),
                ))
            } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                if json || push {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak commit --json` and `oak commit --push` are not supported inside mounts yet; use `oak mount finish <path> --desc-file <file> --json`"
                            .to_string(),
                    ))
                } else {
                    // Mounted commits don't go through the local working tree, so
                    // local hooks (which would run against the wrong files) are
                    // intentionally not invoked here.
                    commands::mount::commit_paths(&dest, &cwd, &paths)
                }
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
            branch,
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
                    commands::review::merge_preview_branch_json(&cwd, branch.as_deref())
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
                rt.block_on(commands::merge::run(
                    &cwd,
                    r#continue,
                    abort,
                    branch.as_deref(),
                    wait.map(|minutes| std::time::Duration::from_secs(minutes.max(1) * 60)),
                    force,
                    json,
                ))
            }
        }

        Commands::Switch {
            name,
            create,
            clean,
            discard,
            detach,
        } => {
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
                    match name {
                        Some(name) => commands::switch::create(&cwd, &name, policy),
                        None => commands::switch::fresh(&cwd, policy),
                    }
                }
            } else if discard {
                Err(oak_core::OakError::Io(std::io::Error::other(
                    "switch --discard requires -c; use `oak switch NAME --clean` to require a clean switch to an existing branch",
                )))
            } else {
                let policy = commands::switch::WorktreePolicy::from_clean_flag(clean);
                commands::switch::run_with_policy(&cwd, name.as_deref(), detach, policy)
            }
        }
        Commands::Close {
            name,
            remote,
            json,
            reason,
        } => {
            let name = name.unwrap_or_else(|| {
                if let Ok(ctx) = oak_cli::resolve::resolve(&cwd) {
                    if let Ok(repo) = ctx.open() {
                        if let Ok(Some(n)) = repo.get_current_branch_name() {
                            return n;
                        }
                    }
                }
                eprintln!("No branch name provided and no current branch set");
                std::process::exit(1);
            });
            let close_reason = match reason {
                None => Ok(None),
                Some(value) => oak_core::CloseReason::parse(&value).map(Some),
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
        Commands::Desc { description, file } => {
            read_desc_input("oak desc", description, file).and_then(|description| {
                if let Some(dest) = mount_dest_for_cwd(&cwd) {
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
            } else if json {
                rt.block_on(commands::finish::run_json(&cwd, &description))
                    .and_then(|result| output::print_json(&result))
            } else {
                rt.block_on(commands::finish::run(&cwd, &description))
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
                        paths,
                    }) => match commands::review::DiffMode::parse(&diff_mode) {
                        Err(err) => Err(err),
                        Ok(diff_mode) => {
                            let options = commands::review::DiffJsonOptions {
                                changed_files_limit,
                                changed_files_offset,
                                hunks,
                                max_bytes,
                                context: unified.unwrap_or(oak_core::DEFAULT_CONTEXT_LINES),
                                force_text: text,
                            };
                            if remote {
                                if json || diff_json {
                                    rt.block_on(commands::review::remote_branch_diff_json(
                                        &cwd, &name, &against, diff_mode, &paths, options,
                                    ))
                                } else {
                                    Err(oak_core::OakError::InvalidArgument(
                                        "`oak branch diff --remote` currently requires --json"
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
                    }) => {
                        if remote {
                            if json || review_json {
                                rt.block_on(commands::review::remote_branch_review_json(
                                    &cwd,
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
                            )
                        } else {
                            Err(oak_core::OakError::InvalidArgument(
                                "`oak branch review` currently requires --json".to_string(),
                            ))
                        }
                    }
                    Some(BranchCommands::Rename { old_name, new_name }) => {
                        if json {
                            Err(oak_core::OakError::InvalidArgument(
                                "`oak branch rename` does not support --json".to_string(),
                            ))
                        } else {
                            commands::branch::rename_branch(&cwd, &old_name, &new_name)
                        }
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
            } => {
                if !json {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak agent state` currently requires --json".to_string(),
                    ))
                } else if let Some(dest) = mount_dest_for_cwd(&cwd) {
                    commands::mount::agent_state_json(&dest, refresh, compact)
                } else {
                    rt.block_on(commands::status::run_agent_state_json(
                        &cwd, refresh, compact,
                    ))
                }
            }
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
        } => commands::restore::run(&cwd, &paths, source.as_deref(), force),

        Commands::Diff {
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
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Mount diffs have no revision-endpoint support yet; a
                // branch name here would silently become an empty path
                // filter, the exact quiet failure endpoint parsing
                // eliminates elsewhere. Fail loudly instead.
                let forced = positionals_after_double_dash().unwrap_or(0);
                let unknown = paths
                    .iter()
                    .take(paths.len().saturating_sub(forced))
                    .find(|path| !cwd.join(path).exists());
                if let Some(arg) = unknown {
                    Err(oak_core::OakError::InvalidArgument(format!(
                        "'{}' is not an existing path; revision endpoints (branches/commits) are not supported inside a mount yet. For a path that no longer exists, use `oak diff -- {}`",
                        arg.display(),
                        arg.display()
                    )))
                } else if branch {
                    Err(oak_core::OakError::InvalidArgument(
                        "oak diff --branch is not supported inside a mount; use `oak branch diff <branch>` instead".to_string(),
                    ))
                } else if exit_code
                    || against.is_some()
                    || mode.is_some()
                    || hunks
                    || unified.is_some()
                    || word_diff
                    || text
                {
                    Err(oak_core::OakError::InvalidArgument(
                        "oak diff --exit-code/--against/--mode/--hunks/--unified/--word-diff/--text are not supported inside a mount yet".to_string(),
                    ))
                } else if json {
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
                };
                // `--exit-code`: report "differences exist" as exit 1 without
                // making callers parse any output, matching git's convention.
                match commands::diff::dispatch(&cwd, request) {
                    // Checked exit: report any recorded stdout loss first.
                    Ok(true) if exit_code => output::exit_process(1),
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
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                if json && compact {
                    commands::mount::status_compact_json(&dest)
                } else if json {
                    commands::mount::status_json(&dest)
                } else if porcelain || short {
                    commands::mount::status_porcelain(&dest)
                } else {
                    commands::mount::status(&dest)
                }
            } else if json {
                commands::status::run_json(&cwd, compact)
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

        Commands::Log {
            limit,
            verbose,
            oneline,
            json,
            paths,
            search,
            search_regex,
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Checked before the JSON split so `--json` cannot bypass
                // it: filtering isn't implemented for mounts, and answering
                // a filtered query with unfiltered history (exit 0) would be
                // a lie. JSON callers get the standard error envelope,
                // exit 2.
                if !paths.is_empty() || search.is_some() || search_regex.is_some() {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak log <path>` / `oak log -S` / `oak log -G` are not supported inside mounts yet"
                            .to_string(),
                    ))
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

        Commands::Serve {
            dir,
            port,
            host,
            token,
        } => {
            let target = if dir.is_absolute() {
                dir
            } else {
                cwd.join(dir)
            };
            rt.block_on(commands::serve::run(target, host, port, token))
        }

        Commands::Push {
            remote,
            force,
            repo,
            json,
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Inside a mount, route to mount push. The mount config
                // already knows the owner/repo/remote, so `remote`, `force`,
                // and `repo` flags from the top-level are ignored here.
                if json {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak push --json` is not supported inside mounts yet; use `oak finish --json` for a structured mount publication result"
                            .to_string(),
                    ))
                } else {
                    rt.block_on(commands::mount::push(&dest))
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
                    rt.block_on(commands::push::run(
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
        } => {
            // Inside a mount, pull the trunk into the virtual branch (and drive
            // its conflict-resolution state machine) instead of running the
            // regular working-tree pull, which has no `.oak/` repo to operate on.
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                rt.block_on(commands::mount::pull::run(&dest, force, r#continue, abort))
            } else {
                rt.block_on(commands::pull::run(
                    &cwd,
                    remote.as_deref(),
                    force,
                    r#continue,
                    abort,
                ))
            }
        }

        Commands::Fetch { remote } => rt.block_on(commands::fetch::run(&cwd, remote.as_deref())),

        Commands::Reset { path, force } => commands::reset::run(&cwd, path.as_deref(), force),

        Commands::Clone {
            remote,
            name,
            dest,
            branch,
            shallow,
            allow_unverified_integrity,
            allow_legacy_scope,
            paths,
            // `--full` is a deprecated no-op now that full history is the
            // default; accepted only so old invocations keep parsing.
            full: _,
        } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
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
                Some(name) => {
                    // If `name` looks like a git remote URL, hand off to the git
                    // converter instead of treating it as an oak `owner/repo` spec.
                    if commands::git_clone::looks_like_git_url(&name) {
                        if branch.is_some() {
                            Err(oak_core::OakError::Io(std::io::Error::other(
                                "clone --branch is only supported for Oak repositories",
                            )))
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
                        match rt.block_on(commands::repo::clone_repo_sparse_on_branch_with_policy(
                            &remote,
                            &name,
                            &full_dest,
                            shallow,
                            sparse.clone(),
                            branch.as_deref(),
                            commands::integrity::CloneIntegrityPolicy {
                                allow_legacy_scope,
                                allow_unverified_budget: allow_unverified_integrity,
                            },
                        )) {
                            Ok(()) => {
                                if let Some(branch) = branch {
                                    commands::switch::run(&full_dest, Some(&branch), false)
                                } else {
                                    Ok(())
                                }
                            }
                            Err(e) => Err(e),
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

        Commands::Site { action } => match action {
            SiteCommands::Enable {
                repo,
                remote,
                branch,
                source,
            } => (|| {
                let remote = remote_arg_or_env(remote.as_deref())?;
                rt.block_on(commands::site::enable(
                    &cwd,
                    repo.as_deref(),
                    remote.as_deref(),
                    branch.as_deref(),
                    source.as_deref(),
                ))
            })(),
            SiteCommands::Disable {
                organization,
                remote,
            } => (|| {
                let remote = remote_arg_or_env(remote.as_deref())?;
                rt.block_on(commands::site::disable(
                    &cwd,
                    organization.as_deref(),
                    remote.as_deref(),
                ))
            })(),
            SiteCommands::Show {
                organization,
                remote,
            } => (|| {
                let remote = remote_arg_or_env(remote.as_deref())?;
                rt.block_on(commands::site::show(
                    &cwd,
                    organization.as_deref(),
                    remote.as_deref(),
                ))
            })(),
            SiteCommands::List { remote } => (|| {
                let remote = remote_arg_or_env(remote.as_deref())?;
                rt.block_on(commands::site::list(&cwd, remote.as_deref()))
            })(),
        },

        Commands::Sparse { action, json } => {
            use commands::sparse::SparseAction;
            let action = match action {
                None => SparseAction::List { json },
                Some(SparseCommands::List { json: sub }) => {
                    SparseAction::List { json: json || sub }
                }
                Some(SparseCommands::Set { paths }) => SparseAction::Set { paths },
                Some(SparseCommands::Add { paths }) => SparseAction::Add { paths },
                Some(SparseCommands::Disable) => SparseAction::Disable,
            };
            commands::sparse::run(&cwd, action)
        }

        Commands::Archive { output } => commands::archive::run(&cwd, output.as_deref()),

        Commands::Ci { action } => match action {
            CiCommands::Runs { limit, json } => rt.block_on(commands::ci::runs(&cwd, limit, json)),
            CiCommands::Status { json } => match rt.block_on(commands::ci::status(&cwd, json)) {
                // `oak ci status` communicates the gate state through its
                // exit code (0 success / 1 failure / 3 running) so scripts
                // can branch without parsing output.
                Ok(0) => Ok(()),
                // Checked exit: report any recorded stdout loss first.
                Ok(code) => output::exit_process(code),
                Err(e) => Err(e),
            },
            CiCommands::Wait {
                run_ids,
                commit,
                timeout,
                json,
            } => match rt.block_on(commands::ci::wait(
                &cwd,
                &run_ids,
                commit.as_deref(),
                std::time::Duration::from_secs(timeout),
                json,
            )) {
                Ok(0) => Ok(()),
                Ok(code) => output::exit_process(code),
                Err(e) => Err(e),
            },
            CiCommands::Logs { run_id, json } => {
                rt.block_on(commands::ci::logs(&cwd, run_id, json))
            }
            CiCommands::Rerun { run_id, json } => {
                rt.block_on(commands::ci::rerun(&cwd, run_id, json))
            }
        },

        Commands::CiFetch {
            remote,
            commit,
            token,
            dest,
        } => {
            let dest = dest.unwrap_or_else(|| PathBuf::from("."));
            rt.block_on(commands::ci_fetch::run(&remote, &commit, &token, &dest))
        }

        Commands::Export {
            dest,
            branch,
            git_branch,
            force,
        } => commands::export::run(&cwd, &dest, branch.as_deref(), git_branch.as_deref(), force),

        Commands::Upgrade { force, canary } => rt.block_on(commands::upgrade::run(force, canary)),

        Commands::Release { action } => {
            if let Err(e) = require_cli_feature(oak_core::features::Feature::Releases) {
                output::error(&e);
                std::process::exit(1);
            }
            match action {
                ReleaseCommands::New {
                    tag,
                    title,
                    notes,
                    commit,
                    draft,
                    prerelease,
                } => rt.block_on(commands::release::new_release(
                    &cwd,
                    &tag,
                    title.as_deref(),
                    notes.as_deref(),
                    commit.as_deref(),
                    draft,
                    prerelease,
                )),
                ReleaseCommands::List => rt.block_on(commands::release::list(&cwd)),
                ReleaseCommands::Show { tag } => rt.block_on(commands::release::show(&cwd, &tag)),
                ReleaseCommands::Edit {
                    tag,
                    title,
                    notes,
                    draft,
                    prerelease,
                } => rt.block_on(commands::release::edit(
                    &cwd,
                    &tag,
                    title.as_deref(),
                    notes.as_deref(),
                    draft,
                    prerelease,
                )),
                ReleaseCommands::Publish { tag } => {
                    rt.block_on(commands::release::publish(&cwd, &tag))
                }
                ReleaseCommands::Upload { tag, files } => {
                    rt.block_on(commands::release::upload(&cwd, &tag, &files))
                }
                ReleaseCommands::DeleteAsset { tag, filename } => {
                    rt.block_on(commands::release::delete_asset(&cwd, &tag, &filename))
                }
                ReleaseCommands::Delete { tag } => {
                    rt.block_on(commands::release::delete(&cwd, &tag))
                }
            }
        }

        Commands::Login { remote } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            commands::login::run(&remote)
        })(),

        Commands::Logout { remote } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            commands::logout::run(&remote)
        })(),

        Commands::Whoami { remote } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            commands::whoami::run(&remote)
        })(),

        Commands::Open => commands::open::run(&cwd),

        // `--remote`/`--json` are combined from both positions, so
        // `oak feedback --json --remote X link fb-1` and
        // `oak feedback link fb-1 --json --remote X` are identical.
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
        } => commands::split::run(&cwd, from.as_deref(), plan.as_deref(), dry_run),

        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        Commands::Mount {
            action,
            spec,
            dest,
            branch,
            remote,
        } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            // `oak mount` is always available — it is no longer behind the
            // `OAK_FEATURES=mount` gate.
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
                Some(MountCommands::Forget { dest, force }) => {
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    commands::mount::forget(&abs_dest, force)
                }
                Some(MountCommands::Serve {
                    remote,
                    spec,
                    dest,
                    branch,
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
                    commands::mount::worktree::worktree_create(&remote, &spec)
                })(),
                Some(MountCommands::WorktreeRemove) => commands::mount::worktree::worktree_remove(),
                #[cfg(target_os = "macos")]
                Some(MountCommands::FskitBroker) => commands::mount::fskit::broker::run(),
                #[cfg(not(target_os = "macos"))]
                Some(MountCommands::FskitBroker) => {
                    eprintln!("__fskit-broker is only available on macOS");
                    std::process::exit(1);
                }
                #[cfg(target_os = "macos")]
                Some(MountCommands::FskitBrokerPing { text }) => {
                    commands::mount::fskit::broker::ping(&text).map_err(oak_core::OakError::Server)
                }
                #[cfg(not(target_os = "macos"))]
                Some(MountCommands::FskitBrokerPing { .. }) => {
                    eprintln!("__fskit-broker-ping is only available on macOS");
                    std::process::exit(1);
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
                                    ))
                                }
                                Err(e) => Err(e),
                            }
                        }
                    }
                }
            }
        })(),

        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        Commands::Space { action } => match action {
            SpaceCommands::New { spec, dest, remote } => (|| {
                let remote = remote_arg_or_env_or_default(remote.as_deref())?;
                rt.block_on(commands::spaces::new(&spec, dest.as_deref(), &cwd, &remote))
            })(),
            SpaceCommands::Repos { org, remote } => (|| {
                let remote = remote_arg_or_env_or_default(remote.as_deref())?;
                rt.block_on(commands::spaces::repos(org.as_deref(), &cwd, &remote))
            })(),
            SpaceCommands::Clean { dest, force } => {
                commands::spaces::clean(dest.as_deref(), &cwd, force)
            }
        },

        Commands::Skill { action } => match action {
            SkillCommands::Install { global } => commands::skill::install(global, &cwd),
        },
    };

    // A non-BrokenPipe stdout write failure (EIO, disk full behind a
    // redirect) means output was lost even if the operation succeeded;
    // surface it through the normal error path. A vanished pipe reader is
    // NOT an error — see `output::write_stdout`.
    let result = result.and_then(|()| output::stdout_write_result());

    if let Err(e) = result {
        let code = exit_code(&e);
        // A command that already wrote its full JSON payload (the merge
        // dry-run's verdict document) must not follow it with a second JSON
        // document; its error goes to stderr and the exit code carries the
        // signal. Everything that failed BEFORE emitting a payload still
        // gets an envelope, so a `--json` command never exits with an empty
        // stdout. "Emitted" means the write landed: a payload whose write
        // was lost leaves the marker clear and takes the envelope branch.
        if json_error_envelope && !output::json_payload_emitted() {
            let envelope = output::JsonErrorEnvelope::from_error_with_path(&e, Some(&cwd));
            if let Err(print_err) = output::print_json_error_envelope(&envelope) {
                output::error(&print_err.to_string());
            }
            // The envelope IS the error report, and it goes to *stdout*. If
            // stdout was genuinely unwritable the caller received nothing at
            // all, so leave through the checked exit: it reports the loss on
            // stderr (where a JSON consumer can still see it) and keeps the
            // error's own nonzero code. A vanished pipe reader records
            // nothing and is unaffected.
            output::exit_process(code);
        }
        // The non-JSON error report itself is stderr-only, so it cannot be
        // lost here. What makes skipping the stdout-health check safe is the
        // exit code, not an absence of earlier stdout writes: `exit_code`
        // never returns 0, so this path cannot claim success and an unreported
        // stdout loss can never be mistaken for a clean run. (Plenty of
        // commands do write stdout before failing into this branch — `oak
        // reset` prints the whole discard list, then can fail materializing
        // it. The report the user is entitled to is the error, and that is on
        // stderr.) Re-reporting a recorded stdout error here would also print
        // it twice whenever `e` *is* that error, arriving from the epilogue
        // chain above.
        output::error(&e.to_string());
        std::process::exit(code);
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

fn remote_arg_or_env_or_default(remote: Option<&str>) -> oak_core::Result<String> {
    Ok(remote_arg_or_env(remote)?.unwrap_or_else(|| commands::push::DEFAULT_REMOTE.to_string()))
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

/// Print one help line. The `colors` constants render as nothing when stdout
/// isn't a terminal (same gate as all command output — see `output`), so the
/// line arrives here already plain or already colored.
fn help_line(s: &str) {
    output::print_line(s);
}

fn print_help() {
    use output::colors::*;

    let version = env!("CARGO_PKG_VERSION");

    // Monochrome wordmark banner. The old green tree-glyph mark is retired
    // — the oak symbol is now the compass mark used on oak.space, which a
    // terminal can't render faithfully, so the banner leads with the
    // wordmark instead.
    help_line(&format!("{BOLD}Oak VCS{RESET}"));
    help_line(&format!("{DIM}v{version}{RESET}\n"));
    help_line(&format!(
        "{WHITE}{BOLD}Usage:{RESET} oak {DIM}[--verbose]{RESET} <command> {DIM}[flags] [args]{RESET}\n"
    ));

    // Each command group is tinted by the domain it acts on, so related
    // commands read as a block and the color itself hints at what's affected:
    //   BRIGHT_GREEN  the day-one happy path
    //   GREEN         local working tree / repo creation
    //   CYAN          read-only inspection (history)
    //   YELLOW        branch manipulation (moves HEAD around)
    //   BLUE          talks to the server / network
    //   MAGENTA       mounts & virtual filesystems
    //   WHITE         misc utilities
    // The group heading is tinted the same color as its commands.

    // The baseline flow — what most people want on day one.
    help_line(&format!("{BRIGHT_GREEN}{BOLD}Getting started{RESET}"));
    help_line(&format!(
        "  {BRIGHT_GREEN}oak init{RESET}     Create a repository in the current directory"
    ));
    help_line(&format!(
        "  {BRIGHT_GREEN}oak commit{RESET}   Checkpoint your work locally"
    ));
    help_line(&format!(
        "  {BRIGHT_GREEN}oak push --repo ORG/REPO{RESET}  Publish to a specific org/repo"
    ));

    // Full command reference, grouped by intent. `print_group` prefixes each
    // heading with a blank line, so the spacing stays uniform without manual
    // `println!()`s between groups.
    print_group("Start a repo", GREEN);
    print_cmd("init", "[PATH]", "Initialize a repository", GREEN);
    print_cmd(
        "clone",
        "[ORG/REPO] [--shallow] [--allow-unverified-integrity] [--allow-legacy-scope]",
        "Clone a repo, or pick one",
        GREEN,
    );

    print_group("Snapshot changes", GREEN);
    print_cmd(
        "status",
        "[--json|--porcelain|--short]",
        "Show working tree status",
        GREEN,
    );
    print_cmd("info", "--json", "Show repo/branch metadata", GREEN);
    print_cmd(
        "agent state",
        "--json [--compact] [--refresh]",
        "Show agent-oriented preflight state",
        GREEN,
    );
    print_cmd(
        "conflict",
        "status|show --json",
        "Inspect in-progress conflict state",
        GREEN,
    );
    print_cmd(
        "diff",
        "[PATHS] [--json|--print|--stat|--name-only]",
        "Browse working-tree changes vs HEAD",
        GREEN,
    );
    print_cmd(
        "commit",
        "[--push] [--json --quiet]",
        "Checkpoint locally; --push publishes explicitly",
        GREEN,
    );
    print_cmd(
        "restore",
        "[PATHS] [-s SRC]",
        "Restore files from HEAD or a commit",
        GREEN,
    );
    print_cmd("reset", "[PATH] [-f]", "Discard uncommitted changes", GREEN);

    print_group("History", CYAN);
    print_cmd(
        "log",
        "[-n N] [-v|--oneline] [--json]",
        "Show commit history",
        CYAN,
    );
    print_cmd("hash", "", "Print the current HEAD commit hash", CYAN);

    print_group("Branches", YELLOW);
    print_cmd(
        "branch",
        "[list|show|diff|review|rename] [--json]",
        "List, inspect, review, or rename branches",
        YELLOW,
    );
    print_cmd(
        "switch",
        "[-c] [NAME] [--clean] [-d]",
        "Switch, create, or detach a branch",
        YELLOW,
    );
    print_cmd(
        "desc",
        "--file FILE",
        "Set current branch description",
        YELLOW,
    );
    print_cmd(
        "finish",
        "--desc-file FILE [--json]",
        "Finalize with preflight, push, and description sync",
        YELLOW,
    );
    print_cmd("close", "[NAME]", "Close a branch", YELLOW);
    print_cmd(
        "split",
        "[--plan FILE|-] [--dry-run]",
        "Split a branch's commits into independent branches",
        YELLOW,
    );
    print_cmd(
        "merge",
        "[--wait[=MIN]] [--continue|--abort|--dry-run --json]",
        "Merge current branch into its parent; --wait rides out the CI gate",
        YELLOW,
    );
    print_group("Sync with the server", BLUE);
    print_cmd("login", "[-r URL]", "Log in to an Oak server", BLUE);
    print_cmd("logout", "[-r URL]", "Log out of an Oak server", BLUE);
    print_cmd("whoami", "[-r URL]", "Show the logged-in username", BLUE);
    print_cmd(
        "push",
        "[-r URL] [-f] [--repo ORG/REPO] [--json]",
        "Push commits to the remote",
        BLUE,
    );
    print_cmd(
        "pull",
        "[--continue|--abort]",
        "Fetch this branch, then merge in parent",
        BLUE,
    );
    print_cmd("fetch", "[-r URL]", "Refresh the local copy of main", BLUE);
    print_cmd(
        "doctor",
        "--repo ORG/REPO [--verify metadata|existence|bytes]",
        "Inspect bounded remote content integrity",
        BLUE,
    );
    print_cmd(
        "blob info",
        "HASH --repo ORG/REPO [--depth N [--branch NAME]]",
        "Inspect bounded target-blob byte evidence (admin)",
        BLUE,
    );
    print_cmd(
        "ci",
        "runs|status|wait|logs|rerun [--json]",
        "Inspect and re-run server CI (the merge gate)",
        BLUE,
    );
    print_sub("runs [--limit N]", "List recent CI runs");
    print_sub("status", "CI state for the current branch head");
    print_sub("wait RUN_ID...", "Wait for exact runs to finish");
    print_sub("logs RUN_ID", "Show a run's step logs");
    print_sub("rerun RUN_ID", "Re-dispatch a run (infra flakes)");

    // Mount (built on macOS, Linux, and Windows). Always advertised — `oak
    // mount` is no longer behind the `mount` feature flag.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        help_line(&format!(
            "\n{MAGENTA}{BOLD}Mount{RESET}  {DIM}(virtual filesystem){RESET}"
        ));
        print_cmd(
            "mount",
            "ORG/REPO [DEST]",
            "Mount a repo as a lazy virtual filesystem",
            MAGENTA,
        );
        print_sub("list", "List active mounts");
        print_sub(
            "finish [DEST] --desc-file FILE [--json]",
            "Commit, push, describe, and end",
        );
        print_sub("end [DEST] [-f]", "Unmount, drop state, and remove the dir");
        print_cmd(
            "space",
            "new ORG [DIR]",
            "Scaffold an agent space for an org (one task per dir)",
            MAGENTA,
        );
        print_sub("repos [ORG]", "List the org's repos to mount for a task");
        print_sub("clean [DIR] [-f]", "Tear down finished mounts in a space");
    }

    print_group("More", WHITE);
    print_cmd(
        "export",
        "DEST",
        "Replay history into a fresh git repo",
        WHITE,
    );
    print_cmd("archive", "[-o OUTPUT]", "Create a zip archive", WHITE);
    print_cmd("open", "", "Open the project in a browser", WHITE);
    print_cmd(
        "feedback",
        "[-m TEXT|--file FILE]",
        "Send feedback or a feature request to the Oak team",
        WHITE,
    );
    print_cmd(
        "skill",
        "install [--global]",
        "Install the bundled agent skill (teaches coding agents Oak)",
        WHITE,
    );
    print_cmd(
        "completions",
        "SHELL",
        "Print a shell-completion script (bash, zsh, …)",
        WHITE,
    );
    print_cmd("upgrade", "[-f] [--canary]", "Upgrade the Oak CLI", WHITE);

    help_line(&format!(
        "\nRun {GREEN}oak <command> --help{RESET} for details on any command.\n"
    ));
    help_line(
        "Exit codes: 0 success; 1 generic; 2 usage; 3 locked; 4 dirty tree; 5 conflicts; 6 network/server/auth; 7 merge prediction uncertified.",
    );
    help_line("");
    help_line(&format!(
        "{WHITE}{BOLD}Docs{RESET}      {GREEN}{UNDERLINE}https://oak.space/docs{RESET}"
    ));
    help_line(&format!(
        "{WHITE}{BOLD}Discord{RESET}   {GREEN}{UNDERLINE}https://oak.space/discord{RESET}"
    ));
}

/// Bold group heading inside the command list, tinted `color` to match the
/// commands beneath it, with a leading blank line to separate it from the
/// previous group.
fn print_group(label: &str, color: output::colors::Color) {
    use output::colors::*;
    help_line(&format!("\n{color}{BOLD}{label}{RESET}"));
}

/// Print one command row. `color` tints the command name so each group reads
/// as a distinct block (see the palette in `print_help`); the argument hint
/// stays dim and the description default.
fn print_cmd(name: &str, args: &str, desc: &str, color: output::colors::Color) {
    use output::colors::*;
    if args.is_empty() {
        help_line(&format!("  {color}{name:<12}{RESET}{:<22} {desc}", ""));
    } else {
        help_line(&format!(
            "  {color}{name:<12}{RESET}{DIM}{args:<22}{RESET} {desc}"
        ));
    }
}

fn print_sub(args: &str, desc: &str) {
    use output::colors::*;
    help_line(&format!("  {:<12}{DIM}{args:<22}{RESET} {desc}", ""));
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Extract the `Clone` variant's `shallow` flag from a parsed arg vector.
    fn parse_clone_shallow(args: &[&str]) -> bool {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Clone { shallow, .. } => shallow,
            _ => panic!("expected the Clone subcommand"),
        }
    }

    fn parse_clone_branch(args: &[&str]) -> Option<String> {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Clone { branch, .. } => branch,
            _ => panic!("expected the Clone subcommand"),
        }
    }

    fn parse_clone_integrity_overrides(args: &[&str]) -> (bool, bool) {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Clone {
                allow_unverified_integrity,
                allow_legacy_scope,
                ..
            } => (allow_unverified_integrity, allow_legacy_scope),
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
            exit_code(&oak_core::OakError::MergeInvariantViolation(
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
            exit_code(&oak_core::OakError::MergePredictionUncertified(
                "no authoritative target head".to_string(),
            )),
            7
        );
    }

    #[test]
    fn merge_base_unavailable_errors_use_conflict_exit_code() {
        assert_eq!(
            exit_code(&oak_core::OakError::NoVerifiedCommonAncestor {
                left: "topic".to_string(),
                right: "main".to_string(),
            }),
            5
        );
        assert_eq!(
            exit_code(&oak_core::OakError::IncompleteManifestData {
                left: "topic".to_string(),
                right: "main".to_string(),
                missing: "abc123".to_string(),
            }),
            5
        );
        assert_eq!(
            exit_code(&oak_core::OakError::IncompleteCommitData {
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

    #[test]
    fn switch_discard_hidden_alias_still_parses_for_compatibility() {
        assert_eq!(
            parse_switch_policy(&["oak", "switch", "-c", "--discard"]),
            (true, false, true)
        );
    }
}
