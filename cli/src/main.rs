use oak_cli::commands;
use oak_cli::output;

use std::io::Read;
use std::path::PathBuf;

use clap::Parser;

mod cli_args;

use cli_args::*;

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
/// `--json` (`oak status --porcelain`). Like JSON mode they must stay free of
/// progress animation and the daily version notice; unlike JSON mode their
/// failures are reported as text on stderr.
fn command_emits_machine_rows(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Status {
            porcelain: true,
            ..
        } | Commands::Status { short: true, .. }
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
    if matches!(e, oak_core::OakError::RepoNotFound) {
        if let Some(root) = cwd.and_then(oak_cli::resolve::git_checkout_root) {
            output::info(&git_checkout_hint(&root));
        }
    }
    std::process::exit(code);
}

/// What to do in a plain git checkout, which Oak does not open as a
/// repository: `oak init --import-git` there imports its history.
fn git_checkout_hint(root: &std::path::Path) -> String {
    format!(
        "{} is a git repository. Run `oak init --import-git` there to import its history into Oak.",
        root.display()
    )
}

/// The one-line refusal for a command that was removed or moved, when that
/// is what `args` invoke (after the global `--verbose`), else `None`. None
/// of these are in the command tree, so they appear in neither help nor
/// completions; any invocation says where the command went and exits 2.
fn removed_command_message(args: &[String]) -> Option<String> {
    let mut words = args.iter().skip(1).filter(|arg| !arg.starts_with('-'));
    let command = words.next()?.as_str();
    let message = match command {
        "ci" | "workflow" => format!(
            "`oak {command}` was removed with Oak CI. Marks replaced it: see `oak mark --help` and https://oak.space/docs/marks"
        ),
        "checkout" => {
            "`oak checkout` was removed. Use `oak switch --detach <commit>`".to_string()
        }
        "rev-parse" => "`oak rev-parse` was removed. Use `oak hash`".to_string(),
        "info" => {
            "`oak info` was removed. Use `oak status --json` or `oak agent state --json`"
                .to_string()
        }
        "archive" => {
            "`oak archive` was removed. Use `oak export --tree-only --at <commit> <dir>`"
                .to_string()
        }
        "refs" | "tree" | "file" => {
            format!("`oak {command} inspect` moved. Use `oak doctor {command}`")
        }
        "blob" => "`oak blob info` moved. Use `oak doctor blob`".to_string(),
        "maintenance" => {
            "`oak maintenance compact` moved. Use `oak doctor compact`".to_string()
        }
        "branch" if words.next().map(String::as_str) == Some("close") => {
            "`oak branch close` was removed. Use `oak close`".to_string()
        }
        "mount" => "`oak mount` was removed. For a parallel task, use a separate shallow clone: `oak clone ORG/REPO --shallow DIR`".to_string(),
        "watch" => "`oak watch` was removed. Commit with `oak commit` and publish with `oak push`".to_string(),
        _ => return None,
    };
    Some(message)
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

    // Removed and moved commands (`oak ci`, `oak checkout`, `oak refs
    // inspect`, ...) are not in the command tree; any invocation of them
    // says where they went in one line and exits 2.
    if let Some(message) = removed_command_message(&args) {
        exit_with_error(
            &oak_core::OakError::InvalidArgument(message),
            argv_requests_json(&args),
            None,
            None,
        );
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
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => exit_with_usage_error(err.format(&mut Cli::command()), json_mode),
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

    // The shell's directory can vanish underneath it (deleted, or replaced by
    // a fresh clone at the same path). That is a user-environment
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
            | Commands::Doctor {
                action: Some(
                    DoctorCommands::File { .. }
                        | DoctorCommands::Refs { .. }
                        | DoctorCommands::Tree { .. }
                ),
                ..
            }
            | Commands::Environment { .. }
            | Commands::Change { .. }
    );
    let structured_output = json_mode || command_emits_machine_rows(&cli.command);
    let skip_version_notice = skip_version_notice || structured_output;
    let branch_remote_retry = branch_remote_retry_command(&cli.command);
    output::configure_activity(structured_output);

    // A view checkout sends its view on every request, so the
    // server answers in the view commits it holds. Commands that create a
    // checkout start from no view: cloning the same repo in full from inside
    // a view checkout must not inherit it.
    let creates_checkout = matches!(cli.command, Commands::Clone(_) | Commands::Init { .. });
    if !creates_checkout {
        if let Some(view) = checkout_view(&cwd) {
            oak_cli::http::set_active_view(Some(view));
        }
    }

    let result = match cli.command {
        Commands::Init {
            path,
            json,
            import_git,
            no_import_git,
        } => {
            let target = path
                .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
                .unwrap_or_else(|| cwd.clone());
            let git = if import_git {
                commands::init::GitHistory::Import
            } else if no_import_git {
                commands::init::GitHistory::Skip
            } else {
                commands::init::GitHistory::Ask
            };
            if json {
                // Never interactive under --json: no prompts, narration on
                // stderr, one receipt of what was created.
                output::route_narration_to_stderr();
                commands::init::run_reporting(&target, false, git).and_then(|outcome| {
                    let mut receipt = serde_json::json!({
                        "path": commands::absolute_path_string(&target),
                        "branch": outcome.branch,
                        "imported_commit_count": outcome.imported_commits,
                        "recommended_next_commands": [
                            oak_cli::contract::NextCommand::StatusJsonCompact.render()
                        ],
                    });
                    if let Some(import) = &outcome.import {
                        receipt["git_import"] = import.to_json();
                    }
                    print_receipt("init", receipt)
                })
            } else {
                commands::init::run_reporting(
                    &target,
                    std::io::IsTerminal::is_terminal(&std::io::stdin()),
                    git,
                )
                .map(|_| ())
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
                rt.block_on(commands::merge::run(
                    &cwd,
                    r#continue,
                    abort,
                    branch.as_deref(),
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
                    .map(|_created| ())
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
                    rt.block_on(commands::branch::edit_current_branch_json(
                        &cwd,
                        &description,
                        retry_command,
                    ))
                    .and_then(|receipt| output::print_json(&receipt))
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
            if json {
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
                publication_after,
            } => {
                if !json {
                    Err(oak_core::OakError::InvalidArgument(
                        "`oak agent state` currently requires --json".to_string(),
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
                    commands::conflict::status_checkout(&cwd)
                } else {
                    commands::conflict::status_checkout_human(&cwd)
                }
            }
            ConflictCommands::Show { json } => {
                if json {
                    commands::conflict::show_checkout(&cwd)
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
                match side {
                    Some(side) if json => commands::conflict::take_checkout_json(&cwd, &path, side),
                    Some(side) => commands::conflict::take_checkout(&cwd, &path, side),
                    None => Err(oak_core::OakError::InvalidArgument(
                        "`oak conflict take` requires exactly one of --ours or --theirs"
                            .to_string(),
                    )),
                }
            }
        },
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
            if json && reconcile {
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

        Commands::Hash => commands::hash::run(&cwd),

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
                rt.block_on(commands::push::plan_json(&cwd, remote.as_deref(), force))
            } else if json {
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

        Commands::Pull {
            remote,
            force,
            r#continue,
            abort,
            json,
            branch_only,
        } => {
            let options = commands::pull::PullOptions { force, branch_only };
            if json && abort {
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
                    if commands::git_clone::looks_like_git_url(&name) {
                        if json {
                            Err(oak_core::OakError::InvalidArgument(
                                "clone --json is only supported for Oak repositories".to_string(),
                            ))
                        } else if branch.is_some() || detached {
                            Err(oak_core::OakError::Io(std::io::Error::other(
                                "clone --branch/--detached is only supported for Oak repositories",
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
            action: None,
            repo,
            remote,
            verify,
            depth,
            max_chunks,
            max_bytes,
            json,
        } => (|| {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            // clap requires --repo when no subcommand is given.
            let repo = repo.unwrap_or_default();
            rt.block_on(commands::integrity::doctor(
                &remote, &repo, verify, depth, max_chunks, max_bytes, json,
            ))
        })(),

        Commands::Doctor {
            action: Some(action),
            ..
        } => run_doctor_subcommand(&cwd, &rt, action),

        Commands::Environment { json } => commands::environment::run(json),

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

        Commands::Mark {
            action,
            repo,
            remote,
            json,
        } => (|| {
            let target =
                commands::access::RepoTarget::resolve(&cwd, repo.as_deref(), remote.as_deref())?;
            use commands::mark;
            match action {
                MarkCommands::List => rt.block_on(mark::list(target, json)),
                MarkCommands::Show { mark: m } => rt.block_on(mark::show(target, &m, json)),
                MarkCommands::Create {
                    name,
                    commit,
                    owner_group,
                    root,
                    requires,
                    approvers,
                    allow_group_rollback,
                    description,
                    followers,
                } => rt.block_on(mark::create(
                    target,
                    mark::CreateArgs {
                        name,
                        commit,
                        owner_group,
                        root,
                        requires,
                        approvers,
                        allow_group_rollback,
                        description,
                        followers,
                    },
                    json,
                )),
                MarkCommands::Move {
                    mark: m,
                    commit,
                    note,
                } => rt.block_on(mark::move_to(
                    target,
                    &m,
                    &commit,
                    note.as_deref(),
                    false,
                    json,
                )),
                MarkCommands::Rollback {
                    mark: m,
                    commit,
                    note,
                } => rt.block_on(mark::move_to(
                    target,
                    &m,
                    &commit,
                    note.as_deref(),
                    true,
                    json,
                )),
                MarkCommands::Log { mark: m, limit } => {
                    rt.block_on(mark::log(target, &m, limit, json))
                }
                MarkCommands::Delete { mark: m, yes } => {
                    if !yes {
                        if json || !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                            return Err(oak_core::OakError::InvalidArgument(format!(
                                "deleting {m} drops its moves too; pass --yes"
                            )));
                        }
                        let ok = dialoguer::Confirm::new()
                            .with_prompt(format!("Delete {m} and its moves?"))
                            .default(false)
                            .interact()
                            .unwrap_or(false);
                        if !ok {
                            return Ok(());
                        }
                    }
                    rt.block_on(mark::delete(target, &m, json))
                }
                MarkCommands::Report {
                    commit,
                    name,
                    body,
                    body_file,
                    url,
                    move_id,
                } => {
                    let body = commands::report::read_body(body, body_file.as_deref())?;
                    rt.block_on(commands::report::file(
                        target,
                        commands::report::ReportArgs {
                            commit,
                            name,
                            body,
                            url,
                            move_id,
                        },
                        json,
                    ))
                }
                MarkCommands::Follow {
                    mark: m,
                    run,
                    script_args,
                    dir,
                    state_file,
                    once,
                    no_checkout,
                    no_grant,
                    report_name,
                    wait,
                } => rt.block_on(mark::follow(
                    target,
                    mark::FollowArgs {
                        mark: m,
                        run: std::iter::once(run).chain(script_args).collect(),
                        dir,
                        state_file,
                        once,
                        checkout: !no_checkout,
                        no_grant,
                        report_name,
                        wait_seconds: wait.clamp(1, 55),
                    },
                    json,
                )),
            }
        })(),

        Commands::Report {
            commit,
            name,
            body,
            body_file,
            url,
            move_id,
            repo,
            remote,
            json,
        } => (|| {
            let target =
                commands::access::RepoTarget::resolve(&cwd, repo.as_deref(), remote.as_deref())?;
            use commands::report;
            if body.is_none() && body_file.is_none() {
                return rt.block_on(report::list(target, &commit, name.as_deref(), json));
            }
            let body = report::read_body(body, body_file.as_deref())?;
            rt.block_on(report::file(
                target,
                report::ReportArgs {
                    commit,
                    name: name.unwrap_or_default(),
                    body,
                    url,
                    move_id,
                },
                json,
            ))
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

        Commands::Skill { action } => match action {
            SkillCommands::Install { global, json } => commands::skill::install(global, json, &cwd),
            SkillCommands::Status { json } => commands::skill::status(json, &cwd),
            SkillCommands::Show { file } => commands::skill::show(file.as_deref()),
        },

        Commands::Start(StartArgs {
            summary,
            name,
            no_push,
            clean,
            good: _,
            tip: _,
            json,
        }) => rt.block_on(commands::start::run(
            &cwd,
            &summary,
            name.as_deref(),
            commands::start::StartOptions { no_push, clean },
            json,
        )),
    };

    // A command that succeeded but answers through its exit code (`oak diff
    // --exit-code`) asked for that code with
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

/// `oak doctor refs|tree|file|blob|compact`. A refs/tree/file check that
/// finds a problem prints its report and exits 1.
fn run_doctor_subcommand(
    cwd: &std::path::Path,
    rt: &tokio::runtime::Runtime,
    action: DoctorCommands,
) -> oak_core::Result<()> {
    let verdict = match action {
        DoctorCommands::Blob {
            hash,
            repo,
            remote,
            depth,
            branch,
            json,
        } => {
            let remote = remote_arg_or_env_or_default(remote.as_deref())?;
            return rt.block_on(commands::integrity::blob_info_scoped(
                &remote,
                &repo,
                &hash,
                depth,
                branch.as_deref(),
                json,
            ));
        }
        DoctorCommands::Compact => return commands::maintenance::compact(cwd),
        DoctorCommands::File {
            at,
            path,
            max_bytes,
            json,
            remote,
            output: output_file,
        } => {
            if remote {
                rt.block_on(commands::file::inspect_remote(
                    cwd, &at, &path, max_bytes, json,
                ))
            } else {
                commands::file::inspect(cwd, &at, &path, max_bytes, output_file.as_deref(), json)
            }
        }
        DoctorCommands::Refs { max_branches, json } => {
            commands::refs::inspect(cwd, max_branches, json)
        }
        DoctorCommands::Tree {
            at,
            max_files,
            max_bytes,
            json,
        } => commands::tree::inspect(
            cwd,
            &at,
            oak_core::sqlite::tree_inspect::TreeInspectionLimits {
                max_files,
                max_bytes,
            },
            json,
        ),
    };
    if !verdict? {
        output::set_exit_status(1);
    }
    Ok(())
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
///   BLUE     talks to the server: sync, account, groups, marks, sites
///   CYAN     read-only inspection (history, refs, remote peeks)
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
                ("init", "Create a repository here"),
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
                ("start", "Start a task: a new branch, described and pushed"),
                ("switch", "Switch to or create a branch"),
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
                ("pull", "Update this branch, then merge in main"),
                ("fetch", "Refresh the local copy of main"),
            ],
        ),
        (
            "History & inspection",
            CYAN,
            vec![
                ("log", "Show commit history"),
                ("hash", "Print the current HEAD commit hash"),
                ("agent state", "The repo's state and next actions, as JSON"),
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
            "Marks & sites",
            BLUE,
            vec![
                ("mark", "Move and follow marks: named pointers on main"),
                (
                    "report",
                    "Say something about a commit, or read what was said",
                ),
                ("site", "Oak Pages sites served at https://NAME.oak.page/"),
            ],
        ),
    ];

    groups.extend([
        (
            "Storage & health",
            WHITE,
            vec![
                (
                    "doctor",
                    "Check integrity: remote content, local refs, trees, files",
                ),
                (
                    "retention",
                    "Keep only the newest versions of files in a directory",
                ),
            ],
        ),
        (
            "More",
            WHITE,
            vec![
                ("export", "Export history into a git repository"),
                ("environment", "List the environment variables Oak reads"),
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
    fn start_still_accepts_the_retired_good_and_tip_flags() {
        use clap::CommandFactory;
        for args in [
            &["oak", "start", "task", "--good"][..],
            &["oak", "start", "task", "--good=api,web"],
            &["oak", "start", "task", "--tip"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
        let help = Cli::command()
            .find_subcommand_mut("start")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(
            !help.contains("--good") && !help.contains("--tip"),
            "{help}"
        );
    }

    #[test]
    fn mark_create_has_no_legacy_flags() {
        for flag in [
            "--legacy-environment-id",
            "--legacy-environment-name",
            "--legacy-workflow-path",
            "--legacy-workflow-hash",
            "--legacy-policy-version",
            "--legacy-from-workflow",
        ] {
            assert!(
                Cli::try_parse_from(["oak", "mark", "create", "m", flag, "1"]).is_err(),
                "{flag}"
            );
        }
        assert!(Cli::try_parse_from(["oak", "mark", "create", "m"]).is_ok());
    }

    #[test]
    fn report_files_with_a_name_and_body_or_lists_without() {
        assert!(Cli::try_parse_from(["oak", "report", "main"]).is_ok());
        assert!(Cli::try_parse_from(["oak", "report", "main", "--name", "tests"]).is_ok());
        assert!(Cli::try_parse_from([
            "oak",
            "report",
            "main",
            "--name",
            "tests",
            "--body",
            "ok",
            "--url",
            "https://x",
            "--move-id",
            "3",
            "--json",
            "--repo",
            "acme/demo",
            "-r",
            "https://oak.example",
        ])
        .is_ok());
        assert!(
            Cli::try_parse_from(["oak", "report", "main", "--name", "t", "--body-file", "-"])
                .is_ok()
        );
        // A body needs a name; --url and --move-id need a body.
        assert!(Cli::try_parse_from(["oak", "report", "main", "--body", "ok"]).is_err());
        assert!(
            Cli::try_parse_from(["oak", "report", "main", "--name", "t", "--url", "u"]).is_err()
        );
        assert!(Cli::try_parse_from([
            "oak",
            "report",
            "main",
            "--name",
            "t",
            "--body",
            "a",
            "--body-file",
            "f",
        ])
        .is_err());
        // `oak mark report` still files one, and stays out of help.
        assert!(Cli::try_parse_from([
            "oak", "mark", "report", "main", "--name", "tests", "--body", "ok",
        ])
        .is_ok());
        let help = <Cli as clap::CommandFactory>::command()
            .find_subcommand_mut("mark")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(!help.contains("\n  report "), "{help}");
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
                action: None,
                repo,
                remote,
                json,
                ..
            } => {
                assert_eq!(repo.as_deref(), Some("oak/oakspace"));
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
    fn plain_doctor_still_requires_repo() {
        let error = match Cli::try_parse_from(["oak", "doctor", "--json"]) {
            Ok(_) => panic!("plain `oak doctor` needs --repo"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("--repo"), "{error}");
        // A subcommand doesn't, and doesn't take the remote check's flags.
        assert!(Cli::try_parse_from(["oak", "doctor", "refs", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["oak", "doctor", "--repo", "a/b", "refs"]).is_err());
    }

    #[test]
    fn doctor_blob_rejects_depth_beyond_server_bound() {
        let result = Cli::try_parse_from([
            "oak",
            "doctor",
            "blob",
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
    fn privileged_doctor_blob_syntax_is_stable() {
        match Cli::try_parse_from([
            "oak",
            "doctor",
            "blob",
            "abc123",
            "--repo",
            "oak/oakspace",
            "--json",
        ])
        .expect("doctor blob syntax should parse")
        .command
        {
            Commands::Doctor {
                action:
                    Some(DoctorCommands::Blob {
                        hash, repo, json, ..
                    }),
                ..
            } => {
                assert_eq!(hash, "abc123");
                assert_eq!(repo, "oak/oakspace");
                assert!(json);
            }
            _ => panic!("expected Doctor::Blob"),
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
    fn removed_commands_name_their_replacement() {
        let message = |args: &[&str]| {
            let args: Vec<String> = std::iter::once("oak")
                .chain(args.iter().copied())
                .map(String::from)
                .collect();
            removed_command_message(&args)
        };
        for (args, replacement) in [
            (&["checkout", "abcd"][..], "oak switch --detach"),
            (&["rev-parse", "HEAD"], "oak hash"),
            (&["info", "--json"], "oak status --json"),
            (&["archive"], "oak export --tree-only"),
            (&["refs", "inspect"], "oak doctor refs"),
            (&["tree", "inspect", "--at", "HEAD"], "oak doctor tree"),
            (&["file", "inspect"], "oak doctor file"),
            (&["blob", "info", "abc"], "oak doctor blob"),
            (&["maintenance", "compact"], "oak doctor compact"),
            (&["branch", "close", "x"], "oak close"),
            (&["branch", "--json", "close"], "oak close"),
            (&["--verbose", "ci"], "oak mark"),
            (&["mount", "acme/web", "dir"], "oak clone"),
            (&["watch"], "oak commit"),
        ] {
            let text = message(args).unwrap_or_else(|| panic!("{args:?} not refused"));
            assert!(text.contains(replacement), "{args:?}: {text}");
            assert!(!text.contains('\n'), "{args:?}: {text}");
        }
        for args in [
            &["branch", "list"][..],
            &["doctor", "refs"],
            &["close"],
            &["hash"],
            &["switch", "--detach", "abcd"],
        ] {
            assert_eq!(message(args), None, "{args:?}");
        }
    }

    #[test]
    fn switch_discard_hidden_alias_still_parses_for_compatibility() {
        assert_eq!(
            parse_switch_policy(&["oak", "switch", "-c", "--discard"]),
            (true, false, true)
        );
    }
}
