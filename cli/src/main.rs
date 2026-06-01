use oak_cli::commands;
use oak_cli::output;

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

#[derive(Parser)]
#[command(name = "oak")]
#[command(about = "Oak — Branch freely")]
#[command(version)]
#[command(styles = help_styles())]
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

    /// Create a new commit within the current branch.
    ///
    /// Local commits no longer carry messages — branch descriptions are the
    /// source of truth for "what happened." Set the current branch's
    /// description with `oak desc "..."`.
    Commit {
        /// Skip pre-commit and post-commit hooks for this commit
        #[arg(long)]
        no_verify: bool,
    },

    /// Merge the current branch into its parent
    Merge {
        /// Continue a merge after resolving conflicts
        #[arg(long)]
        r#continue: bool,

        /// Abort a merge in progress
        #[arg(long)]
        abort: bool,
    },

    /// Switch to a branch or detach HEAD at a commit. With no name, prompts
    /// you to pick a branch interactively.
    Switch {
        /// Branch name or commit hash. Omit to pick a branch interactively.
        name: Option<String>,

        /// Create a new branch and switch to it
        #[arg(short = 'c', long = "create")]
        create: bool,

        /// Detach HEAD at the given commit (treat name as commit hash)
        #[arg(short, long)]
        detach: bool,
    },

    /// Close a branch, defaulting to the current branch
    Close {
        /// Branch name
        name: Option<String>,
    },

    /// Set the current branch description
    Desc {
        /// New description
        description: String,
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

    /// Show changes between working directory and HEAD
    Diff,

    /// Print the current HEAD commit hash
    Hash,

    /// Show the status of the working directory
    Status,

    /// Show commit history
    Log {
        /// Maximum number of commits to show
        #[arg(short = 'n', long)]
        limit: Option<usize>,

        /// Show verbose output including changed files
        #[arg(short, long)]
        verbose: bool,
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
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
        remote: String,

        /// Force push: overwrite remote history even when diverged
        #[arg(short, long)]
        force: bool,

        /// Link a not-yet-linked repo to OWNER/NAME without the interactive
        /// org picker. OWNER must be an existing organization slug; the repo
        /// is created on the server if it doesn't exist. Lets scripted /
        /// agent pushes (no TTY) link a fresh repo on first push.
        #[arg(long = "repo", value_name = "OWNER/NAME", env = "OAK_REPO")]
        repo: Option<String>,
    },

    /// Bring the local clone fully up to date: fetch new commits on the
    /// current branch from the remote, then merge in any changes from the
    /// parent branch (what `oak sync` used to do).
    Pull {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
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
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
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
    /// opens an interactive repo/project picker.
    #[command(visible_alias = "get")]
    Clone {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
        remote: String,

        /// Repository spec in `owner/name` form (e.g. `oak/oak`). A bare
        /// `name` is allowed when logged in — it defaults to your personal
        /// organization (your username as owner). Omit to search and pick.
        name: Option<String>,

        /// Destination directory. Defaults to the repo name in the current
        /// directory (git-style — `oak clone oak/foo` clones into `./foo`).
        dest: Option<PathBuf>,

        /// Scope the clone to a team's projects. Only paths under the union
        /// of the team's project `path_prefix` values are fetched and
        /// materialized. Mutually exclusive with `--project`.
        #[arg(long, value_name = "SLUG", conflicts_with = "project")]
        team: Option<String>,

        /// Scope the clone to a single project. Only paths under the
        /// project's `path_prefix` are fetched and materialized.
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,

        /// Download the full commit history. By default `oak clone` is a
        /// shallow clone — it fetches only the most recent commit on the
        /// default branch (like `git clone --depth=1`), which is much faster
        /// and is all you need for most work. Pass `--full` to get everything.
        #[arg(long)]
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

    /// Upgrade oak to the latest version
    Upgrade {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
        remote: String,

        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Tag management commands
    Tag {
        #[command(subcommand)]
        action: TagCommands,
    },

    /// Manage GitHub-style releases on the server (notes + downloadable artifacts)
    Release {
        #[command(subcommand)]
        action: ReleaseCommands,
    },

    /// Log in to an Oak server
    Login {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
        remote: String,
    },

    /// Print a compact repo-status segment for your shell prompt.
    ///
    /// Renders one line like `● my-feature +2 ~1 -1` summarizing the current
    /// branch and working-tree changes — meant to be embedded in your prompt
    /// (`oak login` offers to wire this up). Prints nothing outside an Oak
    /// repo and never errors, so it's safe to run on every prompt render.
    Prompt {
        /// Target shell, so color escapes are wrapped to not count toward the
        /// prompt width (`bash` uses readline markers, `zsh` uses `%{ %}`).
        /// Omit to print raw ANSI for previewing in a terminal.
        #[arg(long, value_name = "SHELL")]
        shell: Option<String>,

        /// Disable color output (also honors NO_COLOR / OAK_NO_COLOR).
        #[arg(long)]
        no_color: bool,
    },

    /// Open the project in the web browser
    Open,

    /// Show a visual tree of branches based on their parent links
    Tree,

    /// Open a SQLite shell to the repository database (read-write).
    ///
    /// Arbitrary writes can corrupt this repository — only proceed if you
    /// know what you're doing.
    Query,

    /// Mount a remote repository as a virtual filesystem.
    ///
    /// Files are downloaded lazily on access — useful for very large repos
    /// where a full clone is impractical. Writes happen on a virtual branch
    /// named `<dest-slug>--<id8>` (derived from the mount directory's
    /// leaf name) that lives only locally until you push it.
    ///
    /// macOS requires macFUSE (https://osxfuse.github.io) or fuse-t.
    ///
    /// Shorthand: `oak mount <repo> [branch]` — creates/reuses an agent
    /// space at `./<repo>` (or `./<repo>-<branch>` if needed), then mounts
    /// the named branch (or the repo's default head branch when omitted)
    /// inside that space.
    #[cfg(all(
        feature = "mount",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    ))]
    Mount {
        #[command(subcommand)]
        action: Option<MountCommands>,

        /// Repository spec in `organization/repo` form. Used by the shorthand
        /// invocation `oak mount <repo> [branch]` when no subcommand is given.
        spec: Option<String>,

        /// Branch to mount (used with the shorthand invocation). Defaults to
        /// the repo's head branch (preferring `main`, then `master`).
        branch: Option<String>,

        /// Optional remote URL for the shorthand invocation.
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
        remote: String,

        /// Scope the mount to a team's projects. Only paths under the
        /// union of the team's project `path_prefix` values appear in the
        /// mount. Mutually exclusive with `--project`.
        #[arg(long, value_name = "SLUG", conflicts_with = "project")]
        team: Option<String>,

        /// Scope the mount to a single project. Only paths under the
        /// project's `path_prefix` appear in the mount.
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
    },
}

#[cfg(all(
    feature = "mount",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
#[derive(Subcommand)]
enum MountCommands {
    /// Mount a remote repository at <dest>. Runs in the foreground until
    /// interrupted (Ctrl-C). Lazily downloads files as they are read.
    Start {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
        remote: String,

        /// Repository spec in `organization/repo` form (e.g. `oak/oak`)
        spec: String,

        /// Destination directory to mount onto. Created if it doesn't exist.
        dest: PathBuf,

        /// Branch to mount (defaults to the repo's default branch)
        #[arg(short, long)]
        branch: Option<String>,

        /// Scope the mount to a team's projects. Mutually exclusive with
        /// `--project`.
        #[arg(long, value_name = "SLUG", conflicts_with = "project")]
        team: Option<String>,

        /// Scope the mount to a single project.
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
    },

    /// List active mounts
    List,

    /// Show status for a mount, or summarize mounts under cwd when DEST is omitted
    Status {
        /// Mount destination directory. Omit to summarize active mounts under cwd.
        dest: Option<PathBuf>,

        /// Print nothing when every mount under cwd is clean. Useful from hooks.
        #[arg(long)]
        quiet: bool,
    },

    /// Push the virtual branch (and dependencies) to the remote
    Push {
        /// Mount destination directory
        dest: PathBuf,
    },

    /// Forget a mount's local state (only safe when nothing is mounted there)
    Forget {
        /// Mount destination directory
        dest: PathBuf,
    },

    /// Unmount, forget local state, and remove the mount directory in one
    /// step. The "I'm done with this task" command.
    End {
        /// Mount destination directory
        dest: PathBuf,

        /// Discard any uncommitted overlay changes. Without this, `end`
        /// refuses to operate on a mount with dirty files so you don't
        /// silently lose work.
        #[arg(short, long)]
        force: bool,
    },

    /// Claude Code `WorktreeCreate` hook: mount <spec> at the worktree path
    /// read from the hook's stdin JSON, then print the path. Wired into an
    /// Oak space's `.claude/settings.json`; not meant to be run by hand.
    #[command(hide = true)]
    WorktreeCreate {
        /// Remote server URL
        #[arg(short, long, env = "OAK_REMOTE", default_value = "https://oakvcs.com")]
        remote: String,

        /// Repository spec in `organization/repo` form (e.g. `oak/oak`)
        spec: String,
    },

    /// Claude Code `WorktreeRemove` hook: unmount + clean up the mount at the
    /// worktree path read from the hook's stdin JSON. Wired into an Oak
    /// space's `.claude/settings.json`; not meant to be run by hand.
    #[command(hide = true)]
    WorktreeRemove,
}

#[derive(Subcommand)]
enum SiteCommands {
    /// Enable Pages-style hosting for an organization, serving the chosen
    /// repo's `main` branch at <organization>.<base_domain>/. Defaults:
    /// source=/. From inside a checkout, the repo and organization are
    /// inferred from the cwd; pass --repo owner/name to override.
    Enable {
        /// Repo to publish (owner/name). Defaults to the repo for the
        /// current directory. The owner is the organization whose site is
        /// being configured.
        #[arg(short, long)]
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
enum TagCommands {
    /// Create a tag pointing to HEAD or a specific commit
    Create {
        /// Tag name
        name: String,

        /// Commit hash (defaults to HEAD)
        commit: Option<String>,
    },

    /// List all tags
    List,

    /// Delete a tag
    Delete {
        /// Tag name
        name: String,
    },
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
    // for it to avoid a redundant prompt. `oak prompt` renders into PS1 on
    // every keystroke-return, so it must emit nothing but the segment itself —
    // a stray "new version available" line would corrupt the prompt.
    let skip_version_notice = matches!(
        cli.command,
        Commands::Upgrade { .. } | Commands::Prompt { .. }
    );
    // The `WorktreeCreate` hook must emit nothing on stdout but the worktree
    // path, and `WorktreeRemove` runs at session teardown — a stray "new
    // version available" line would corrupt the hook output, so suppress the
    // daily notice for both.
    #[cfg(all(
        feature = "mount",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    ))]
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

        Commands::Commit { no_verify } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Mounted commits don't go through the local working tree, so
                // local hooks (which would run against the wrong files) are
                // intentionally not invoked here.
                commands::mount::commit(&dest)
            } else {
                commands::commit::run_with_options(
                    &cwd,
                    commands::commit::CommitOptions { no_verify },
                )
            }
        }

        Commands::Merge { r#continue, abort } => {
            rt.block_on(commands::merge::run(&cwd, r#continue, abort))
        }

        Commands::Switch {
            name,
            create,
            detach,
        } => {
            if create {
                if detach {
                    Err(oak_core::OakError::Io(std::io::Error::other(
                        "switch -c cannot be combined with --detach",
                    )))
                } else {
                    match name {
                        Some(name) => commands::switch::create(&cwd, &name),
                        None => Err(oak_core::OakError::Io(std::io::Error::other(
                            "switch -c requires a branch name",
                        ))),
                    }
                }
            } else {
                commands::switch::run(&cwd, name.as_deref(), detach)
            }
        }
        Commands::Close { name } => {
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
            commands::branch::close_branch(&cwd, &name)
        }
        Commands::Desc { description } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                // Inside a mount, set the description on the virtual branch
                // in the mount cache db (and best-effort-push it to the
                // server). The regular `edit_current_branch` path can't see
                // the virtual branch because it opens the local-repo SQLite
                // rather than the mount cache.
                rt.block_on(commands::mount::desc(&dest, &description))
            } else {
                commands::branch::edit_current_branch(&cwd, &description)
            }
        }
        Commands::Checkout { reference } => commands::checkout::run(&cwd, &reference),

        Commands::Restore {
            paths,
            source,
            force,
        } => commands::restore::run(&cwd, &paths, source.as_deref(), force),

        Commands::Diff => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                commands::mount::diff(&dest)
            } else {
                commands::diff::run(&cwd)
            }
        }

        Commands::Hash => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                commands::mount::hash(&dest)
            } else {
                commands::hash::run(&cwd)
            }
        }

        Commands::Status => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                commands::mount::status(&dest)
            } else {
                commands::status::run(&cwd)
            }
        }

        Commands::Log { limit, verbose } => {
            if let Some(dest) = mount_dest_for_cwd(&cwd) {
                commands::mount::log(&dest, limit)
            } else {
                commands::log::run(&cwd, limit, verbose)
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
            team,
            project,
            full,
        } => match name {
            None => {
                if dest.is_some() || team.is_some() || project.is_some() {
                    output::warning(
                        "Destination and --team/--project flags are ignored when using the interactive clone picker",
                    );
                }
                rt.block_on(commands::repo::clone_interactive(&remote, &cwd, full))
            }
            Some(name) => {
                // If `name` looks like a git remote URL, hand off to the git
                // converter instead of treating it as an oak `owner/repo` spec.
                // Team/project filtering is meaningless for the git path
                // (no Oak project metadata in a git repo) so it's ignored there.
                if commands::git_clone::looks_like_git_url(&name) {
                    if team.is_some() || project.is_some() {
                        output::warning("--team / --project are ignored when cloning a git URL");
                    }
                    commands::git_clone::run(&name, dest, &cwd)
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
                    rt.block_on(commands::repo::clone_repo(
                        &remote,
                        &name,
                        &full_dest,
                        team.as_deref(),
                        project.as_deref(),
                        full,
                    ))
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

        Commands::Upgrade { remote, force } => rt.block_on(commands::upgrade::run(&remote, force)),

        Commands::Tag { action } => match action {
            TagCommands::Create { name, commit } => {
                commands::tag::create(&cwd, &name, commit.as_deref())
            }
            TagCommands::List => commands::tag::list(&cwd),
            TagCommands::Delete { name } => commands::tag::delete(&cwd, &name),
        },

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

        Commands::Prompt { shell, no_color } => {
            // Note: no `mount_dest_for_cwd` routing here — `prompt::run` does
            // its own mount detection and falls back to regular-repo status.
            commands::prompt::run(&cwd, shell.as_deref(), no_color)
        }

        Commands::Open => commands::open::run(&cwd),

        Commands::Tree => commands::tree::run(&cwd),

        Commands::Query => commands::query::run(&cwd),

        #[cfg(all(
            feature = "mount",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        ))]
        Commands::Mount {
            action,
            spec,
            branch,
            remote,
            team,
            project,
        } => {
            // Two shapes are accepted:
            //   1. `oak mount <subcommand>` — long-form, dispatched via `action`.
            //   2. `oak mount <spec> <branch>` — agent-space shorthand. Creates
            //      or reuses `./<repo-leaf>`, then mounts into a child dir.
            match action {
                Some(MountCommands::Start {
                    remote,
                    spec,
                    dest,
                    branch,
                    team,
                    project,
                }) => {
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    rt.block_on(commands::mount::start(
                        &remote,
                        &spec,
                        &abs_dest,
                        branch.as_deref(),
                        team.as_deref(),
                        project.as_deref(),
                    ))
                }
                Some(MountCommands::List) => commands::mount::list(),
                Some(MountCommands::Status { dest, quiet }) => {
                    if let Some(dest) = dest {
                        let abs_dest = if dest.is_absolute() {
                            dest
                        } else {
                            cwd.join(dest)
                        };
                        commands::mount::status(&abs_dest)
                    } else {
                        commands::spaces::status(&cwd, quiet)
                    }
                }
                Some(MountCommands::Push { dest }) => {
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    rt.block_on(commands::mount::push(&abs_dest))
                }
                Some(MountCommands::Forget { dest }) => {
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    commands::mount::forget(&abs_dest)
                }
                Some(MountCommands::End { dest, force }) => {
                    let abs_dest = if dest.is_absolute() {
                        dest
                    } else {
                        cwd.join(dest)
                    };
                    commands::mount::end(&abs_dest, force)
                }
                Some(MountCommands::WorktreeCreate { remote, spec }) => {
                    commands::mount::worktree::worktree_create(&remote, &spec)
                }
                Some(MountCommands::WorktreeRemove) => commands::mount::worktree::worktree_remove(),
                None => {
                    // Shorthand path. `spec` is required; `branch` is optional
                    // and defaults to the repo's head branch (preferring
                    // `main`, then `master`).
                    let Some(spec) = spec else {
                        eprintln!(
                            "error: `oak mount` needs a subcommand or `<spec> [branch]` shorthand"
                        );
                        eprintln!("hint: try `oak mount --help`");
                        std::process::exit(2);
                    };
                    rt.block_on(commands::mount::shorthand_mount(
                        &remote,
                        &spec,
                        branch.as_deref(),
                        team.as_deref(),
                        project.as_deref(),
                        &cwd,
                    ))
                }
            }
        }
    };

    if let Err(e) = result {
        output::error(&e.to_string());
        std::process::exit(1);
    }

    // On the success path, opportunistically check (at most once a day) whether
    // a newer oak is available and, if so, prompt the user to run `oak upgrade`.
    if !skip_version_notice {
        commands::version_check::maybe_notify(&rt);
    }
}

fn print_help() {
    use output::colors::*;

    let version = env!("CARGO_PKG_VERSION");

    // Banner + the public tagline, verbatim, in brand green.
    println!("{BRIGHT_GREEN}{BOLD}↟ Oak{RESET} Branch freely. {DIM}(v{version}){RESET}\n");
    println!(
        "{WHITE}{BOLD}Usage:{RESET} oak {DIM}[--verbose]{RESET} <command> {DIM}[flags] [args]{RESET}\n"
    );

    // The baseline flow — what most people want on day one.
    println!("{WHITE}{BOLD}Getting started{RESET}");
    println!("  {GREEN}oak init{RESET}     Create a repository in the current directory");
    println!("  {GREEN}oak commit{RESET}   Record a snapshot of your work");
    println!("  {GREEN}oak push{RESET}     Publish your branch to the Oak server");

    // Full command reference, grouped by intent. `print_group` prefixes each
    // heading with a blank line, so the spacing stays uniform without manual
    // `println!()`s between groups.
    print_group("Start a repo");
    print_cmd("init", "[PATH]", "Initialize a repository");
    print_cmd(
        "clone",
        "[OWNER/NAME]",
        "Clone a repo, or pick one (alias: get)",
    );

    print_group("Snapshot changes");
    print_cmd("status", "", "Show working tree status");
    print_cmd("diff", "", "Show working-tree changes vs HEAD");
    print_cmd("commit", "", "Record a snapshot on the current branch");
    print_cmd(
        "restore",
        "[PATHS] [-s SRC]",
        "Restore files from HEAD or a commit",
    );
    print_cmd("reset", "[PATH] [-f]", "Discard uncommitted changes");

    print_group("History");
    print_cmd("log", "[-n N] [-v]", "Show commit history");
    print_cmd("hash", "", "Print current HEAD commit hash");
    print_cmd("tree", "", "Show a visual tree of branches");

    print_group("Branches");
    print_cmd(
        "switch",
        "[-c] [NAME] [-d]",
        "Switch, create, or detach a branch",
    );
    print_cmd("desc", "DESC", "Set current branch description");
    print_cmd("close", "[NAME]", "Close a branch");
    print_cmd(
        "merge",
        "[--continue|--abort]",
        "Merge current branch into its parent",
    );
    print_cmd("tag", "create NAME [COMMIT]", "Create a tag");
    print_sub("list", "List all tags");
    print_sub("delete NAME", "Delete a tag");

    print_group("Sync with the server");
    print_cmd("login", "[-r URL]", "Log in to an Oak server");
    print_cmd(
        "push",
        "[-r URL] [-f] [--repo OWNER/NAME]",
        "Push commits to the remote",
    );
    print_cmd(
        "pull",
        "[--continue|--abort]",
        "Fetch this branch, then merge in parent",
    );
    print_cmd("fetch", "[-r URL]", "Refresh the local copy of main");

    // Mount (only built when compiled with --features mount)
    #[cfg(all(
        feature = "mount",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    ))]
    {
        println!(
            "\n{WHITE}{BOLD}Mount{RESET}  {DIM}(virtual filesystem; build with --features mount){RESET}"
        );
        print_cmd(
            "mount",
            "OWNER/REPO [BRANCH]",
            "Set up an agent space and mount a branch",
        );
        print_sub("start SPEC DEST", "Mount with an explicit dest path");
        print_sub("list", "List active mounts");
        print_sub("status [DEST]", "Show a mount, or summarize cwd");
        print_sub("push DEST", "Push the virtual branch to the remote");
        print_sub("forget DEST", "Forget a mount's local state");
        print_sub("end DEST", "Unmount, forget, and remove the dir");
    }

    print_group("More");
    print_cmd(
        "prompt",
        "[--shell SH]",
        "Status segment for your shell prompt",
    );
    print_cmd("export", "DEST", "Replay history into a fresh git repo");
    print_cmd("archive", "[-o OUTPUT]", "Create a zip archive");
    print_cmd("open", "", "Open the project in a browser");
    print_cmd("query", "", "Open a SQLite shell to the repo db");
    print_cmd("upgrade", "[-r URL] [-f]", "Upgrade the Oak CLI");

    println!("\nRun {GREEN}oak <command> --help{RESET} for details on any command.\n");
    println!("{WHITE}{BOLD}Docs{RESET}      {GREEN}{UNDERLINE}https://oakvcs.com/docs{RESET}");
    println!("{WHITE}{BOLD}Discord{RESET}   {GREEN}{UNDERLINE}https://oakvcs.com/discord{RESET}");
}

/// White, bold group heading inside the command list, with a leading blank
/// line to separate it from the previous group.
fn print_group(label: &str) {
    use output::colors::*;
    println!("\n{WHITE}{BOLD}{label}{RESET}");
}

fn print_cmd(name: &str, args: &str, desc: &str) {
    use output::colors::*;
    if args.is_empty() {
        println!("  {GREEN}{name:<12}{RESET}{:<22} {desc}", "");
    } else {
        println!("  {GREEN}{name:<12}{RESET}{DIM}{args:<22}{RESET} {desc}");
    }
}

fn print_sub(args: &str, desc: &str) {
    use output::colors::*;
    println!("  {:<12}{DIM}{args:<22}{RESET} {desc}", "");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Extract the `Clone` variant's `full` flag from a parsed arg vector.
    fn parse_clone_full(args: &[&str]) -> bool {
        match Cli::try_parse_from(args)
            .expect("args should parse")
            .command
        {
            Commands::Clone { full, .. } => full,
            _ => panic!("expected the Clone subcommand"),
        }
    }

    #[test]
    fn clone_is_shallow_by_default() {
        // The new default: `oak clone <repo>` performs a shallow clone, so
        // `full` is false unless the user opts in.
        assert!(!parse_clone_full(&["oak", "clone", "oak/oak"]));
        assert!(!parse_clone_full(&["oak", "clone"]));
    }

    #[test]
    fn clone_full_flag_opts_into_full_history() {
        assert!(parse_clone_full(&["oak", "clone", "oak/oak", "--full"]));
        // Order-independent and composes with other flags.
        assert!(parse_clone_full(&["oak", "clone", "--full", "oak/oak"]));
        assert!(parse_clone_full(&[
            "oak", "clone", "oak/oak", "dest", "--full"
        ]));
    }
}
