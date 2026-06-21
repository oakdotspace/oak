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
  4 dirty working tree blocked the operation
  5 merge/sync conflicts or in-progress conflict state
  6 network, server, or auth failure";

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

        /// Emit machine-readable JSON for --dry-run
        #[arg(long)]
        json: bool,
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

    /// Browse changes between working directory and HEAD
    Diff {
        /// Limit the diff to these files or directories (repo paths,
        /// absolute, or relative to the current directory)
        paths: Vec<PathBuf>,

        /// Emit machine-readable JSON summary
        #[arg(long)]
        json: bool,

        /// Return at most N changed-file summaries in --json output
        #[arg(long, requires = "json")]
        changed_files_limit: Option<usize>,

        /// Start --json changed-file summaries at this offset
        #[arg(long, requires = "json", default_value_t = 0)]
        changed_files_offset: usize,

        /// Print the diff to stdout instead of opening the interactive browser
        #[arg(long, conflicts_with = "name_only")]
        print: bool,

        /// Show per-file +added/-removed line counts instead of full hunks
        #[arg(long, conflicts_with = "name_only")]
        stat: bool,

        /// Show only changed file paths
        #[arg(long, conflicts_with = "json")]
        name_only: bool,
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

    /// Show commit history
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
        /// (like `git log -S`)
        #[arg(short = 'S', long = "search", value_name = "TERM")]
        search: Option<String>,
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

        /// Optional shared bearer token required on every request. When unset,
        /// the server is open (intended for localhost / trusted networks).
        #[arg(long, env = "OAK_SERVE_TOKEN")]
        token: Option<String>,
    },

    /// Push commits to remote server
    Push {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,

        /// Force push: overwrite remote history even when diverged
        #[arg(short, long)]
        force: bool,

        /// Link a not-yet-linked repo to ORG/REPO without the interactive
        /// org picker. ORG must be an existing organization slug; the repo
        /// is created on the server if it doesn't exist. Lets scripted /
        /// agent pushes (no TTY) link a fresh repo on first push.
        #[arg(long = "repo", value_name = "ORG/REPO", env = "OAK_REPO")]
        repo: Option<String>,
    },

    /// Bring the local clone fully up to date: fetch new commits on the
    /// current branch from the remote, then merge in any changes from the
    /// parent branch (what `oak sync` used to do).
    Pull {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,

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
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,
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
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,

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

        /// Clone only the most recent commit on the default branch (like
        /// `git clone --depth=1`) instead of the entire history. The working
        /// tree is identical either way — only the locally-stored history
        /// differs — so this is purely a download-speed/disk optimization. By
        /// default `oak clone` downloads the full commit history.
        #[arg(long)]
        shallow: bool,

        /// Deprecated no-op: full history is now the default. Accepted so
        /// existing `oak clone --full` invocations keep working.
        #[arg(long, hide = true, conflicts_with = "shallow")]
        full: bool,
    },

    /// Static-site (Pages-style) management
    Site {
        #[command(subcommand)]
        action: SiteCommands,
    },

    /// Create a zip archive of the current directory
    Archive {
        /// Output file path (defaults to <directory_name>.zip)
        #[arg(short, long)]
        output: Option<PathBuf>,
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
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,
    },

    /// Log out of an Oak server
    Logout {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,
    },

    /// Print the logged-in username for an Oak server
    Whoami {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,
    },

    /// Open the project in the web browser
    Open,

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
    ///   oak mount                  mount every repo you can see under
    ///                              ~/oaktree/<org>/<repo>
    ///   oak mount <org>            mount every repo in that org under ~/oaktree
    ///   oak mount <org>/<repo>     mount that repo at ./<repo>
    ///   oak mount <org>/<repo> <dest>   mount that repo at <dest>
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

        /// What to mount: `<org>` (every repo in the org) or
        /// `<org>/<repo>` (one repo). Omit to mount every repo you can see.
        #[arg(value_name = "ORG/REPO")]
        spec: Option<String>,

        /// Destination directory for a single `<org>/<repo>` mount.
        /// Defaults to `./<repo>`.
        dest: Option<PathBuf>,

        /// Remote server URL.
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,
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

        /// Review the configured remote branch without switching to it
        #[arg(long)]
        remote: bool,
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

        /// Review the configured remote branch without switching to it
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
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,
    },

    /// List the repos in the space's org so you can pick which to mount for a
    /// task. With no ORG, reads the `.oak-space` marker from the current
    /// directory (or an ancestor).
    Repos {
        /// Org slug to list. Defaults to the current space's org.
        #[arg(value_name = "ORG")]
        org: Option<String>,

        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,
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
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,

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
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oak.space")]
        remote: String,

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
        #[arg(long, env = "OAK_REMOTE")]
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
        #[arg(long, env = "OAK_REMOTE")]
        remote: Option<String>,
    },

    /// Show the site config for an organization.
    Show {
        /// Organization slug to operate on. Defaults to the organization owning
        /// the current directory's repo.
        #[arg(short, long)]
        organization: Option<String>,
        /// Remote server URL
        #[arg(long, env = "OAK_REMOTE")]
        remote: Option<String>,
    },

    /// List all organization sites the caller can see.
    List {
        /// Remote server URL
        #[arg(long, env = "OAK_REMOTE")]
        remote: Option<String>,
    },
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
        Commands::Merge { dry_run, json, .. } => *dry_run && *json,
        Commands::Commit { json, .. } | Commands::Finish { json, .. } => *json,
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
        | Commands::Finish { json: true, .. } => true,
        Commands::Merge {
            dry_run: true,
            json: true,
            ..
        } => true,
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

/// Stable Oak CLI exit-code contract:
/// 0 success; 1 generic error; 2 usage error; 3 repository locked/retryable;
/// 4 dirty working tree blocked the operation; 5 merge/sync conflicts or an
/// in-progress conflict state; 6 network/server/auth failure.
fn exit_code(err: &oak_core::OakError) -> i32 {
    use oak_core::OakError;

    match err {
        OakError::InvalidArgument(_) | OakError::InvalidPath(_) => 2,
        OakError::RepoLocked => 3,
        OakError::DirtyWorkingTree(_) | OakError::UncommittedChanges => 4,
        OakError::ConflictDetected
        | OakError::LocalCommitsNotInRemoteHistory
        | OakError::RemoteCommitsNotInLocalHistory
        | OakError::MergeConflict(_)
        | OakError::MergeInProgress => 5,
        OakError::Http(_)
        | OakError::Server(_)
        | OakError::R2(_)
        | OakError::RemoteRepoNotFound(_)
        | OakError::RemoteRepoAlreadyExists(_)
        | OakError::FinishPhaseFailed(_)
        | OakError::CommitPhaseFailed(_) => 6,
        OakError::FinishPreflight(details) if details.blocker == "invalid_description" => 2,
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
        return;
    }

    let cli = Cli::parse();

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
    let skip_version_notice = matches!(cli.command, Commands::Upgrade { .. });
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
                } else {
                    commands::review::merge_preview_branch_json(&cwd, branch.as_deref())
                }
            } else if json {
                Err(oak_core::OakError::InvalidArgument(
                    "`oak merge --json` requires --dry-run".to_string(),
                ))
            } else {
                rt.block_on(commands::merge::run(
                    &cwd,
                    r#continue,
                    abort,
                    branch.as_deref(),
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
            if create {
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
                        remote,
                    }) => match commands::review::DiffMode::parse(&diff_mode) {
                        Err(err) => Err(err),
                        Ok(diff_mode) => {
                            if remote {
                                if json || diff_json {
                                    rt.block_on(commands::review::remote_branch_diff_json(
                                        &cwd,
                                        &name,
                                        &against,
                                        diff_mode,
                                        changed_files_limit,
                                        changed_files_offset,
                                    ))
                                } else {
                                    Err(oak_core::OakError::InvalidArgument(
                                        "`oak branch diff --remote` currently requires --json"
                                            .to_string(),
                                    ))
                                }
                            } else if json || diff_json {
                                commands::review::branch_diff_json(
                                    &cwd,
                                    &name,
                                    &against,
                                    diff_mode,
                                    changed_files_limit,
                                    changed_files_offset,
                                )
                            } else {
                                Err(oak_core::OakError::InvalidArgument(
                                    "`oak branch diff` currently requires --json".to_string(),
                                ))
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
            json,
            changed_files_limit,
            changed_files_offset,
            print,
            stat,
            name_only,
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
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
            } else if json {
                commands::review::worktree_diff_json(
                    &cwd,
                    &paths,
                    changed_files_limit,
                    changed_files_offset,
                )
            } else if print || stat || name_only {
                commands::diff::run(&cwd, &paths, stat, name_only)
            } else {
                commands::diff::run_tui(&cwd, &paths)
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
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                if json {
                    commands::mount::log_json(&dest, limit)
                } else if !paths.is_empty() || search.is_some() {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak log <path>` / `oak log -S` are not supported inside mounts yet"
                            .to_string(),
                    ))
                } else {
                    commands::mount::log(&dest, limit, oneline)
                }
            } else if json {
                commands::log::run_json(&cwd, limit, &paths, search.as_deref())
            } else {
                commands::log::run(&cwd, limit, verbose, oneline, &paths, search.as_deref())
            }
        }

        Commands::Serve { dir, port, token } => {
            let target = if dir.is_absolute() {
                dir
            } else {
                cwd.join(dir)
            };
            rt.block_on(commands::serve::run(target, port, token))
        }

        Commands::Push {
            remote,
            force,
            repo,
        } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Inside a mount, route to mount push. The mount config
                // already knows the owner/repo/remote, so `remote`, `force`,
                // and `repo` flags from the top-level are ignored here.
                rt.block_on(commands::mount::push(&dest))
            } else {
                rt.block_on(commands::push::run(&cwd, &remote, force, repo.as_deref()))
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
                rt.block_on(commands::pull::run(&cwd, &remote, force, r#continue, abort))
            }
        }

        Commands::Fetch { remote: _ } => rt.block_on(commands::fetch::run(&cwd)),

        Commands::Reset { path, force } => commands::reset::run(&cwd, path.as_deref(), force),

        Commands::Clone {
            remote,
            name,
            dest,
            branch,
            shallow,
            // `--full` is a deprecated no-op now that full history is the
            // default; accepted only so old invocations keep parsing.
            full: _,
        } => match name {
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
                    rt.block_on(commands::repo::clone_interactive(&remote, &cwd, shallow))
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
                    match rt.block_on(commands::repo::clone_repo(
                        &remote, &name, &full_dest, shallow,
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
        },

        Commands::Site { action } => match action {
            SiteCommands::Enable {
                repo,
                remote,
                branch,
                source,
            } => rt.block_on(commands::site::enable(
                &cwd,
                repo.as_deref(),
                remote.as_deref(),
                branch.as_deref(),
                source.as_deref(),
            )),
            SiteCommands::Disable {
                organization,
                remote,
            } => rt.block_on(commands::site::disable(
                &cwd,
                organization.as_deref(),
                remote.as_deref(),
            )),
            SiteCommands::Show {
                organization,
                remote,
            } => rt.block_on(commands::site::show(
                &cwd,
                organization.as_deref(),
                remote.as_deref(),
            )),
            SiteCommands::List { remote } => {
                rt.block_on(commands::site::list(&cwd, remote.as_deref()))
            }
        },

        Commands::Archive { output } => commands::archive::run(&cwd, output.as_deref()),

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

        Commands::Login { remote } => commands::login::run(&remote),

        Commands::Logout { remote } => commands::logout::run(&remote),

        Commands::Whoami { remote } => commands::whoami::run(&remote),

        Commands::Open => commands::open::run(&cwd),

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
            remote,
        } => {
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
                }) => {
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
                }
                Some(MountCommands::Resume { dest }) => {
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    rt.block_on(commands::mount::serve_resume(&abs_dest))
                }
                Some(MountCommands::WorktreeCreate { remote, spec }) => {
                    commands::mount::worktree::worktree_create(&remote, &spec)
                }
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
                    // No subcommand: the spec decides what to mount.
                    //   (none)        → every repo, under ~/oaktree
                    //   <owner>       → every repo in that org, under ~/oaktree
                    //   <owner>/<repo> [dest] → one repo at <dest> (or ./<repo>)
                    match spec {
                        None => rt.block_on(commands::mount::mount_all(&remote, None)),
                        Some(spec) if !spec.contains('/') => {
                            rt.block_on(commands::mount::mount_all(&remote, Some(&spec)))
                        }
                        Some(spec) => {
                            let abs_dest =
                                dest.map(|d| if d.is_absolute() { d } else { cwd.join(d) });
                            rt.block_on(commands::mount::mount_one(&remote, &spec, abs_dest, &cwd))
                        }
                    }
                }
            }
        }

        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        Commands::Space { action } => match action {
            SpaceCommands::New { spec, dest, remote } => {
                rt.block_on(commands::spaces::new(&spec, dest.as_deref(), &cwd, &remote))
            }
            SpaceCommands::Repos { org, remote } => {
                rt.block_on(commands::spaces::repos(org.as_deref(), &cwd, &remote))
            }
            SpaceCommands::Clean { dest, force } => {
                commands::spaces::clean(dest.as_deref(), &cwd, force)
            }
        },
    };

    if let Err(e) = result {
        if json_error_envelope {
            let envelope = output::JsonErrorEnvelope::from_error_with_path(&e, Some(&cwd));
            if let Err(print_err) = output::print_json_error_envelope(&envelope) {
                output::error(&print_err.to_string());
            }
        } else {
            output::error(&e.to_string());
        }
        std::process::exit(exit_code(&e));
    }

    // On the success path, opportunistically check (at most once a day) whether
    // a newer oak is available and, if so, prompt the user to run `oak upgrade`.
    if !skip_version_notice {
        commands::version_check::maybe_notify(&rt);
    }
}

/// Print one help line. The `colors` constants render as nothing when stdout
/// isn't a terminal (same gate as all command output — see `output`), so the
/// line arrives here already plain or already colored.
fn help_line(s: &str) {
    println!("{s}");
}

fn print_help() {
    use output::colors::*;

    let version = env!("CARGO_PKG_VERSION");

    // Bordered banner in brand green, with the version on the next line.
    help_line(&format!("{BRIGHT_GREEN}{BOLD}↟↟↟↟↟↟↟↟↟↟↟{RESET}"));
    help_line(&format!("{BRIGHT_GREEN}{BOLD}↟ Oak VCS ↟{RESET}"));
    help_line(&format!("{BRIGHT_GREEN}{BOLD}↟↟↟↟↟↟↟↟↟↟↟{RESET}"));
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
    print_cmd("clone", "[ORG/REPO]", "Clone a repo, or pick one", GREEN);

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
        "[--continue|--abort|--dry-run --json]",
        "Merge current branch into its parent",
        YELLOW,
    );
    print_group("Sync with the server", BLUE);
    print_cmd("login", "[-r URL]", "Log in to an Oak server", BLUE);
    print_cmd("logout", "[-r URL]", "Log out of an Oak server", BLUE);
    print_cmd("whoami", "[-r URL]", "Show the logged-in username", BLUE);
    print_cmd(
        "push",
        "[-r URL] [-f] [--repo ORG/REPO]",
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
    print_cmd("upgrade", "[-f] [--canary]", "Upgrade the Oak CLI", WHITE);

    help_line(&format!(
        "\nRun {GREEN}oak <command> --help{RESET} for details on any command.\n"
    ));
    help_line(
        "Exit codes: 0 success; 1 generic; 2 usage; 3 locked; 4 dirty tree; 5 conflicts; 6 network/server/auth.",
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
