#![allow(dead_code)]

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use oak_core::{Branch, BranchStatus, ChangeType, Commit, FileChange, FileStatus};
use serde::Serialize;

pub const COMPACT_CHANGE_LIMIT: usize = 20;

// ---------------------------------------------------------------------------
// Verbose / timing instrumentation
// ---------------------------------------------------------------------------

static VERBOSE: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();
static STRUCTURED_OUTPUT: AtomicBool = AtomicBool::new(false);
static JSON_PAYLOAD_EMITTED: AtomicBool = AtomicBool::new(false);

/// Record that a command already wrote its full JSON payload to stdout.
///
/// Commands that print a payload and *then* fail (today: `oak merge
/// --dry-run --json`, which prints the merge-safety verdict before exiting
/// non-zero on a blocking or uncertified verdict) use this so the top-level
/// error handler does not append a second JSON document to stdout. It is
/// deliberately NOT set by [`print_json`] itself: the suppression must be an
/// explicit statement that the payload IS the JSON document for this run,
/// never an accident of having printed something earlier.
///
/// **Precondition: the payload write actually succeeded.** Suppressing the
/// error envelope is only sound when stdout really carries the payload; a
/// payload that never reached the reader must NOT silence the envelope, or a
/// failing `--json` run ends with neither a document on stdout nor a
/// diagnosis anywhere. So this performs an explicit flush and leaves the
/// marker clear when the flush fails: a buffered write that fails at flush
/// time never reached the reader.
///
/// The check lives HERE, not at the call site, so no caller can bypass it.
///
/// On this branch the flush is the whole check, and that is both sound and
/// complete: the stdout write funnel is `println!`, which PANICS on a write
/// error, so the payload either lands or the process dies loudly. A
/// "silently lost payload" state does not exist pre-merge.
///
/// # MERGE OBLIGATION — fb-106 (oak/oak @ mrmrs-e80c25, head
/// `3f70ce6e8f121db7449e391453d4de64824d481954e9b9092483bcdf331c7362`)
///
/// The branch name is for readability only; it is mutable, so the binding
/// identity is the full head above. This obligation was written and
/// VERIFIED against exactly that head, where fb-106 declares `static
/// STDOUT_GONE: AtomicBool = AtomicBool::new(false);` at
/// `cli/src/output.rs:236` and `pub fn stdout_write_result() ->
/// oak_core::Result<()>` at `cli/src/output.rs:306`. If fb-106 has advanced
/// past that head by merge time, the merger must re-check that
/// `STDOUT_GONE` still exists and still means the same thing — a sticky
/// "stdout writes are being swallowed" flag — before applying the guard
/// below; everything here is stated against that head, not against whatever
/// the branch name currently points at.
///
/// fb-106 replaces the panicking funnel with one that turns stdout writes
/// into silent no-ops after a failure and reports the loss at exit. That
/// makes "the payload write was silently lost" reachable, and the flush
/// alone stops detecting it: once the funnel has swallowed the write there
/// is nothing left buffered, so the flush succeeds and the marker is set for
/// a payload the reader never got — restoring the unconditional suppression
/// this guard exists to prevent.
///
/// The merge must add EXACTLY ONE condition to this function: after the
/// flush, return early WITHOUT setting the marker when fb-106's sticky
/// lost-write flag `STDOUT_GONE` is set. The merged body in full:
///
/// ```text
/// pub fn mark_json_payload_emitted() {
///     if std::io::stdout().flush().is_err() {
///         return;
///     }
///     if STDOUT_GONE.load(Ordering::Relaxed) {
///         return;
///     }
///     JSON_PAYLOAD_EMITTED.store(true, Ordering::Relaxed);
/// }
/// ```
///
/// The three added lines are the `STDOUT_GONE` guard. Nothing else changes —
/// in particular `merge_preview_branch_json` keeps its ordering (print
/// payload, then mark, then return the verdict error), and the guard stays
/// inside this function rather than moving to the call site.
///
/// Read `STDOUT_GONE`, NOT fb-106's `stdout_write_result()`. The question
/// asked here is "did the payload reach the reader", and that accessor
/// deliberately returns `Ok` for `BrokenPipe` — the one write failure
/// fb-106 does not surface as an error, yet still a payload the reader never
/// got. Consulting the accessor would suppress the error envelope on exactly
/// the shape this guard is for.
///
/// Three test-merge obligations come with it, all MANDATORY MERGE GATES —
/// the merge is not complete until every one of them holds, and the merger
/// must not skip any of them:
///
/// 1. this branch's marker-reset helper (`tests::reset_stdout_markers`) must
///    clear `STDOUT_GONE` as well as `JSON_PAYLOAD_EMITTED`, or one test
///    that trips the flag leaves every later marker test seeing a lost
///    stdout.
/// 2. fb-106's `stdout_failures_silence_writes_and_only_real_errors_surface`
///    must take this branch's `STDOUT_MARKER_LOCK` and call
///    `reset_stdout_markers()` on entry: it asserts `!STDOUT_GONE` at entry,
///    and after the merge these marker tests write that flag too.
/// 3. a union-only test for the LOST-PAYLOAD CELL must land in the same
///    change as the guard: it sets `STDOUT_GONE`, calls
///    [`mark_json_payload_emitted`], and asserts the marker stays CLEAR, so
///    that a payload the reader never got still lets the error envelope
///    fire. This is the cell the guard exists for, and it is untested on
///    either branch alone: there is no unit test here for the flush-failure
///    branch, because forcing `stdout().flush()` to fail needs the process's
///    stdout fd closed or exhausted, which is process-wide and not
///    constructible under the test harness. Post-merge the equivalent guard
///    IS directly testable by setting `STDOUT_GONE`, so the guard shipping
///    without that test is an incomplete merge, not a deferred nicety.
pub fn mark_json_payload_emitted() {
    if std::io::stdout().flush().is_err() {
        // The payload never landed, so it is not this run's JSON document.
        // Leave the marker clear: the error envelope (and any end-of-run
        // report of the lost output) must still happen.
        return;
    }
    if STDOUT_GONE.load(Ordering::Relaxed) {
        return;
    }
    JSON_PAYLOAD_EMITTED.store(true, Ordering::Relaxed);
}

/// True once [`mark_json_payload_emitted`] has been called this process
/// *and* the payload it marked actually reached stdout.
///
/// Pre-payload failures (bad flags, not a repository, an unreadable branch)
/// leave this false, so those still get the standard JSON error envelope —
/// a `--json` command always emits exactly one JSON document. A payload
/// whose write was lost leaves it false for the same reason.
pub fn json_payload_emitted() -> bool {
    JSON_PAYLOAD_EMITTED.load(Ordering::Relaxed)
}

/// Enable verbose timing output. Picks up the `OAK_VERBOSE=1` env var as well.
pub fn enable_verbose() {
    VERBOSE.store(true, Ordering::Relaxed);
    START.get_or_init(Instant::now);
}

/// Returns true if verbose output is enabled (via flag or `OAK_VERBOSE`).
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Emit a timestamped verbose line to stderr (no-op if verbose is off).
/// Format: `[+1.234s] message` with the elapsed wall time since `enable_verbose()`.
pub fn vlog(msg: &str) {
    if !is_verbose() {
        return;
    }
    let elapsed = START.get_or_init(Instant::now).elapsed();
    eprintln!(
        "{}[+{:>7.3}s]{} {}",
        colors::DIM.for_stderr(),
        elapsed.as_secs_f64(),
        colors::RESET.for_stderr(),
        msg
    );
}

// ---------------------------------------------------------------------------
// Human activity feedback
// ---------------------------------------------------------------------------

/// Configure whether this command's output is machine-structured.
///
/// Structured commands (`--json`, porcelain, compact rows, hook protocols) are
/// never allowed to animate, even when `OAK_PROGRESS=always`, because Oak is an
/// agent-first CLI and those streams must stay parseable and token-efficient.
pub fn configure_activity(structured_output: bool) {
    STRUCTURED_OUTPUT.store(structured_output, Ordering::Relaxed);
}

/// A short-lived human progress indicator.
///
/// The handle is intentionally cheap to create at command/phase boundaries. In
/// non-interactive, captured, CI, or structured-output contexts it becomes a
/// no-op, so command code can express activity without carrying output policy.
pub struct Activity {
    bar: Option<indicatif::ProgressBar>,
}

impl Activity {
    pub fn set_message(&self, message: impl Into<String>) {
        if let Some(bar) = &self.bar {
            bar.set_message(message.into());
        }
    }

    pub fn inc(&self, delta: u64) {
        if let Some(bar) = &self.bar {
            bar.inc(delta);
        }
    }

    pub fn finish_and_clear(mut self) {
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

/// Start an animated spinner for an unknown-duration phase.
pub fn activity(message: impl Into<String>) -> Activity {
    if !activity_enabled() {
        return Activity { bar: None };
    }

    let bar = indicatif::ProgressBar::new_spinner();
    bar.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    bar.set_style(activity_style());
    bar.set_message(message.into());
    bar.enable_steady_tick(Duration::from_millis(100));
    Activity { bar: Some(bar) }
}

/// Start a determinate progress bar that follows the same suppression rules as
/// [`activity`]. This is the migration path for command-local `indicatif` bars.
pub fn activity_bar(total: u64, message: impl Into<String>) -> Activity {
    if !activity_enabled() {
        return Activity { bar: None };
    }

    let bar = indicatif::ProgressBar::new(total);
    bar.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    bar.set_style(
        indicatif::ProgressStyle::with_template(
            "  {spinner:.cyan} {msg} [{bar:30.cyan/dim}] {pos}/{len}",
        )
        .unwrap()
        .progress_chars("=>-")
        .tick_strings(spinner_frames()),
    );
    bar.set_message(message.into());
    bar.enable_steady_tick(Duration::from_millis(100));
    Activity { bar: Some(bar) }
}

fn activity_style() -> indicatif::ProgressStyle {
    indicatif::ProgressStyle::with_template("  {spinner:.cyan} {msg}")
        .unwrap()
        .tick_strings(spinner_frames())
}

fn activity_enabled() -> bool {
    activity_enabled_for(
        STRUCTURED_OUTPUT.load(Ordering::Relaxed),
        capture_active(),
        progress_mode(),
        std::env::var_os("CI").is_some(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProgressMode {
    Auto,
    Never,
    Always,
}

fn progress_mode() -> ProgressMode {
    match std::env::var("OAK_PROGRESS") {
        Ok(value) => progress_mode_from_str(&value),
        Err(_) => ProgressMode::Auto,
    }
}

fn progress_mode_from_str(value: &str) -> ProgressMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "never" | "off" | "false" | "0" => ProgressMode::Never,
        "always" | "on" | "true" | "1" => ProgressMode::Always,
        _ => ProgressMode::Auto,
    }
}

fn activity_enabled_for(
    structured_output: bool,
    capture: bool,
    mode: ProgressMode,
    ci: bool,
    stdout_tty: bool,
    stderr_tty: bool,
) -> bool {
    if structured_output || capture {
        return false;
    }

    match mode {
        ProgressMode::Never => false,
        ProgressMode::Always => true,
        ProgressMode::Auto => !ci && stdout_tty && stderr_tty,
    }
}

fn spinner_frames() -> &'static [&'static str] {
    match std::env::var("OAK_SPINNER")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "line" => &["-", "\\", "|", "/"],
        "pulse" => &[".", "o", "O", "o"],
        "arc" => &["◜", "◠", "◝", "◞", "◡", "◟"],
        "minimal" => &[".", "..", "...", "...."],
        "oak" => &["oak   ", "oak.  ", "oak.. ", "oak..."],
        _ => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    }
}

// ---------------------------------------------------------------------------
// Thread-local output capture (used by oak-mcp to call CLI functions directly)
// ---------------------------------------------------------------------------

thread_local! {
    static CAPTURE_BUFFER: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Start capturing output to an in-memory buffer instead of stdout.
pub fn begin_capture() {
    CAPTURE_BUFFER.with(|buf| {
        *buf.borrow_mut() = Some(Vec::new());
    });
}

/// Stop capturing and return all lines joined by newlines.
pub fn end_capture() -> String {
    CAPTURE_BUFFER.with(|buf| buf.borrow_mut().take().unwrap_or_default().join("\n"))
}

fn capture_active() -> bool {
    CAPTURE_BUFFER.with(|buf| buf.borrow().is_some())
}

/// Whether stdout writes have failed once; when set, [`write_stdout`] is a
/// silent no-op for the rest of the process.
static STDOUT_GONE: AtomicBool = AtomicBool::new(false);

/// The first non-`BrokenPipe` stdout write failure, surfaced at command end
/// via [`stdout_write_result`]. `BrokenPipe` is never recorded here — a
/// vanished reader is not an error.
static STDOUT_ERROR: OnceLock<std::io::Error> = OnceLock::new();

/// Internal: write to stdout, treating a vanished reader as "done", not death.
///
/// `println!` panics when the read end of stdout goes away mid-stream —
/// `oak diff --print | head -1` used to die with "failed printing to stdout:
/// Broken pipe (os error 32)". For a CLI, a consumer that stops reading is
/// not an error, and it is also not a reason to abandon the command: `oak
/// reset --force x | head` must still reset `x`. So the first stdout write
/// failure flips a sticky process-global flag and every later write becomes
/// a silent no-op — the command runs to completion and exits with whatever
/// its operation's outcome is (0 on success, so `set -o pipefail` scripts
/// never see a spurious failure).
///
/// Why handle `ErrorKind::BrokenPipe` here instead of resetting `SIGPIPE` to
/// `SIG_DFL` at startup (the git approach): Rust starts with `SIGPIPE`
/// ignored, so *every* `EPIPE` in the process — including network sockets —
/// surfaces as an ordinary io error that reqwest/hyper turn into a reported
/// request failure. Resetting to `SIG_DFL` would forfeit that: on Linux,
/// hyper's vectored socket writes go down std's `writev(2)` path, which
/// (unlike its scalar `send(MSG_NOSIGNAL)` path) raises `SIGPIPE`, so a
/// peer-reset connection could kill `oak push`/`oak pull` mid-transfer with
/// no error report at all. Handling `EPIPE` at this one stdout choke point
/// keeps network I/O fully protected, behaves identically on Windows (which
/// has no `SIGPIPE`), and — unlike a 141 signal death — lets a mutating
/// command finish its work. Every stdout print in this crate funnels through
/// here — enforced by the `disallowed-macros` clippy config in
/// `cli/clippy.toml`.
///
/// A non-`BrokenPipe` failure (EIO, disk full behind a redirect) also stops
/// further writes, but is recorded and returned by [`stdout_write_result`]
/// so `main` reports it through the normal `OakError` path instead of
/// silently claiming success while output was lost.
fn write_stdout(s: &str, newline: bool) {
    use std::io::Write as _;

    if STDOUT_GONE.load(Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout().lock();
    // Stdout is a `LineWriter`: the trailing `\n` (or the explicit flush for
    // newline-less prompt fragments) forces the buffer down the pipe, so a
    // closed pipe surfaces here, on this write, not on some later one.
    let result = if newline {
        writeln!(out, "{s}")
    } else {
        write!(out, "{s}").and_then(|()| out.flush())
    };
    if let Err(e) = result {
        note_stdout_error(e);
    }
}

/// Internal: record a stdout write failure and silence all later writes.
fn note_stdout_error(e: std::io::Error) {
    STDOUT_GONE.store(true, Ordering::Relaxed);
    if e.kind() != std::io::ErrorKind::BrokenPipe {
        let _ = STDOUT_ERROR.set(e);
    }
}

/// The command epilogue's view of stdout health: `Ok` when stdout stayed
/// writable or merely lost its reader (`BrokenPipe`), `Err` when output was
/// genuinely lost to an I/O failure. `main` chains this onto a successful
/// command result so the loss is reported (exit code 1) instead of masked.
pub fn stdout_write_result() -> oak_core::Result<()> {
    match STDOUT_ERROR.get() {
        None => Ok(()),
        Some(e) => Err(oak_core::OakError::Io(std::io::Error::new(
            e.kind(),
            format!("failed writing to stdout: {e}"),
        ))),
    }
}

/// Terminate the process with `code` after the stdout-health check.
///
/// Every path that ends the process without returning through `main`'s
/// normal `Result` epilogue must exit through here instead of calling
/// `std::process::exit` directly: help/version rendering (`oak --help`,
/// clap's subcommand help) and data-code exits (`oak diff --exit-code`,
/// `oak ci status`). If a genuine (non-`BrokenPipe`) stdout write failure
/// was recorded, the output the caller is entitled to never arrived, so the
/// loss is reported on stderr and a would-be success becomes exit `1`
/// (`OakError::Io`'s code in the CLI exit-code contract). Nonzero data codes
/// keep their value — the code itself still answers the caller's question —
/// but the loss is still reported. A vanished pipe reader records nothing
/// (see [`write_stdout`]) and changes nothing here.
pub fn exit_process(code: i32) -> ! {
    if let Err(e) = stdout_write_result() {
        error(&e.to_string());
        if code == 0 {
            std::process::exit(1);
        }
    }
    std::process::exit(code);
}

/// Whether an interactive prompt can actually reach the user — the gate every
/// confirmation must consult before asking a question.
///
/// A prompt is a *two-stream* operation: the question leaves on stdout, the
/// answer arrives on stdin. Gating on stdin alone is therefore not enough. A
/// TTY stdin paired with a redirected — or already dead — stdout means the
/// question is invisible while the answer is still accepted, so a destructive
/// command can be "confirmed" by someone who never saw what they confirmed.
///
/// Before the stdout funnel existed, a `print!` + `flush()?` prompt at least
/// aborted on an unwritable stdout. The funnel deliberately swallows that
/// failure so commands run to completion (see [`write_stdout`]), which is
/// right for output but wrong for a question — hence this explicit gate.
///
/// "Usable" means both streams are terminals *and* no stdout write failure has
/// been recorded: a stdout that already lost a write silently cannot be
/// trusted to carry the question. Callers that can't prompt must refuse the
/// operation with an error naming `--force`, never fall through and act.
pub fn can_prompt() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && !STDOUT_GONE.load(Ordering::Relaxed)
}

/// Internal: write a complete line to the capture buffer or to stdout.
///
/// Lines arrive here already colored or already plain — the `colors`
/// constants decide at render time (see the color-gating section below) —
/// so this writes them verbatim.
fn emit_line(s: &str) {
    let captured = CAPTURE_BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        if let Some(ref mut lines) = *buf {
            lines.push(s.to_string());
            true
        } else {
            false
        }
    });
    if !captured {
        write_stdout(s, true);
    }
}

/// Internal: write a complete diagnostic line to the capture buffer or to
/// stderr. Warnings and errors go here rather than [`emit_line`] so they
/// never interleave with parseable data on stdout — piping `oak log`/`oak
/// diff` yields data only, while diagnostics stay visible on the terminal.
fn emit_stderr_line(s: &str) {
    let captured = CAPTURE_BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        if let Some(ref mut lines) = *buf {
            lines.push(s.to_string());
            true
        } else {
            false
        }
    });
    if !captured {
        eprintln!("{s}");
    }
}

/// Print a plain line (replaces direct `println!` calls in command modules).
pub fn print_line(msg: &str) {
    emit_line(msg);
}

/// Print to stdout without a trailing newline and flush — for prompt
/// fragments (`"… [y/N] "`) and pre-rendered multi-line blobs (shell
/// completions). Same broken-pipe contract as [`print_line`]: a vanished
/// reader silences output without aborting the command.
pub fn print_raw(msg: &str) {
    write_stdout(msg, false);
}

/// Print a single compact JSON document to stdout.
pub fn print_json<T: Serialize>(value: &T) -> oak_core::Result<()> {
    emit_line(&serde_json::to_string(value)?);
    Ok(())
}

pub fn print_json_error_envelope(value: &JsonErrorEnvelope) -> oak_core::Result<()> {
    print_json(value)
}

#[derive(Debug, Serialize)]
pub struct JsonErrorEnvelope {
    pub schema_version: u32,
    pub error: JsonErrorBody,
}

#[derive(Debug, Serialize)]
pub struct JsonErrorBody {
    pub code: String,
    pub message: String,
    pub conflict_paths: Vec<String>,
    pub recommended_next_commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishErrorJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitErrorJson>,
}

#[derive(Debug, Serialize)]
pub struct FinishErrorJson {
    pub phase: String,
    pub completed_phases: Vec<String>,
    pub pending_phases: Vec<String>,
    pub retry_command: Option<String>,
    pub manual_recovery_commands: Vec<String>,
    pub blocker: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CommitErrorJson {
    pub phase: String,
    pub committed: bool,
    pub pushed: bool,
    pub published: bool,
    pub remote_contacted: bool,
    pub branch: String,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub unpushed_commit_count: usize,
    pub retry_command: Option<String>,
    pub manual_recovery_commands: Vec<String>,
}

impl JsonErrorEnvelope {
    pub fn from_error(err: &oak_core::OakError) -> Self {
        Self::from_error_with_path(err, None)
    }

    pub fn from_error_with_path(err: &oak_core::OakError, cwd: Option<&std::path::Path>) -> Self {
        let (code, recommended_next_commands, finish, commit): (
            &str,
            Vec<String>,
            Option<FinishErrorJson>,
            Option<CommitErrorJson>,
        ) = match err {
            oak_core::OakError::RepoNotFound => (
                "repo_not_found",
                vec!["oak init".to_string(), "oak clone <repo>".to_string()],
                None,
                None,
            ),
            oak_core::OakError::RepoLocked => (
                "repo_locked",
                vec!["oak status --json".to_string()],
                None,
                None,
            ),
            oak_core::OakError::MergeInProgress => (
                "merge_in_progress",
                vec![
                    "oak merge --continue".to_string(),
                    "oak merge --abort".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::MergeConflict(_) | oak_core::OakError::ConflictDetected => (
                "conflict_detected",
                vec![
                    "oak conflict status --json".to_string(),
                    "oak conflict show --json".to_string(),
                    "oak pull --continue".to_string(),
                    "oak pull --abort".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::IncompleteAncestry { .. } => (
                "incomplete_ancestry",
                vec![
                    "oak fetch".to_string(),
                    "oak pull --force".to_string(),
                    "oak status --json".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::IncompleteManifestData { .. } => (
                "incomplete_manifest_data",
                vec![
                    "oak fetch".to_string(),
                    "oak pull --force".to_string(),
                    "oak status --json".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::IncompleteCommitData { .. } => (
                "incomplete_commit_data",
                vec![
                    "oak fetch".to_string(),
                    "oak pull --force".to_string(),
                    "oak status --json".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::IncompleteBlobData { .. } => (
                "incomplete_blob_data",
                vec![
                    "oak fetch".to_string(),
                    "oak pull --force".to_string(),
                    "oak status --json".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::NoVerifiedCommonAncestor { .. } => (
                "no_verified_common_ancestor",
                vec![
                    "oak fetch".to_string(),
                    "oak pull --force".to_string(),
                    "oak status --json".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::UncommittedChanges | oak_core::OakError::DirtyWorkingTree(_) => (
                "dirty_worktree",
                vec!["oak status --json".to_string()],
                None,
                None,
            ),
            oak_core::OakError::InvalidArgument(_) | oak_core::OakError::InvalidPath(_) => {
                ("invalid_argument", Vec::new(), None, None)
            }
            oak_core::OakError::Config(message) => (
                "configuration_error",
                config_recommended_commands(message),
                None,
                None,
            ),
            oak_core::OakError::RemoteNotConfigured => (
                "configuration_error",
                vec![
                    crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string(),
                    "oak info --json".to_string(),
                ],
                None,
                None,
            ),
            oak_core::OakError::FinishPreflight(details) => {
                let commands = finish_recommended_commands(
                    details.retry_command.as_deref(),
                    &details.manual_recovery_commands,
                );
                let pending_phases = if details.pending_phases.is_empty() {
                    vec![
                        "description".to_string(),
                        "commit".to_string(),
                        "push".to_string(),
                        "metadata_sync".to_string(),
                    ]
                } else {
                    details.pending_phases.clone()
                };
                (
                    "finish_preflight_failed",
                    commands,
                    Some(FinishErrorJson {
                        phase: "preflight".to_string(),
                        completed_phases: Vec::new(),
                        pending_phases,
                        retry_command: details.retry_command.clone(),
                        manual_recovery_commands: details.manual_recovery_commands.clone(),
                        blocker: Some(details.blocker.clone()),
                    }),
                    None,
                )
            }
            oak_core::OakError::FinishPhaseFailed(details) => {
                let commands = finish_recommended_commands(
                    details.retry_command.as_deref(),
                    &details.manual_recovery_commands,
                );
                (
                    "finish_phase_failed",
                    commands,
                    Some(FinishErrorJson {
                        phase: details.phase.clone(),
                        completed_phases: details.completed_phases.clone(),
                        pending_phases: details.pending_phases.clone(),
                        retry_command: details.retry_command.clone(),
                        manual_recovery_commands: details.manual_recovery_commands.clone(),
                        blocker: None,
                    }),
                    None,
                )
            }
            oak_core::OakError::CommitPhaseFailed(details) => {
                let mut commands = details.manual_recovery_commands.clone();
                if let Some(retry) = &details.retry_command {
                    if !commands.iter().any(|cmd| cmd == retry) {
                        commands.insert(0, retry.clone());
                    }
                }
                (
                    "commit_phase_failed",
                    commands,
                    None,
                    Some(CommitErrorJson {
                        phase: details.phase.clone(),
                        committed: details.committed,
                        pushed: details.pushed,
                        published: details.published,
                        remote_contacted: details.remote_contacted,
                        branch: details.branch.clone(),
                        head_before: details.head_before.clone(),
                        head_after: details.head_after.clone(),
                        unpushed_commit_count: details.unpushed_commit_count,
                        retry_command: details.retry_command.clone(),
                        manual_recovery_commands: details.manual_recovery_commands.clone(),
                    }),
                )
            }
            oak_core::OakError::Http(_)
            | oak_core::OakError::Server(_)
            | oak_core::OakError::R2(_)
            | oak_core::OakError::RemoteRepoNotFound(_)
            | oak_core::OakError::RemoteRepoAlreadyExists(_) => (
                "remote_error",
                vec!["oak status --json".to_string()],
                None,
                None,
            ),
            _ => ("error", Vec::new(), None, None),
        };
        JsonErrorEnvelope {
            schema_version: crate::work_state::SCHEMA_VERSION,
            error: JsonErrorBody {
                code: code.to_string(),
                message: err.to_string(),
                conflict_paths: cwd
                    .map(crate::work_state::conflict_paths_for_path)
                    .unwrap_or_default(),
                recommended_next_commands,
                finish,
                commit,
            },
        }
    }
}

fn finish_recommended_commands(
    retry_command: Option<&str>,
    manual_recovery_commands: &[String],
) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(command) = retry_command {
        commands.push(command.to_string());
    }
    for command in manual_recovery_commands {
        if !commands.iter().any(|existing| existing == command) {
            commands.push(command.clone());
        }
    }
    if commands.is_empty() {
        commands.push("oak agent state --json".to_string());
    }
    commands
}

fn config_recommended_commands(message: &str) -> Vec<String> {
    let mut commands = Vec::new();
    if message.contains(crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND) {
        push_unique_command(
            &mut commands,
            crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND,
        );
    }
    if let Some(login_command) = backticked_command_starting_with(message, "oak login") {
        push_unique_command(&mut commands, &login_command);
    }
    push_unique_command(&mut commands, "oak info --json");
    commands
}

fn backticked_command_starting_with(message: &str, prefix: &str) -> Option<String> {
    message
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::trim)
        .find(|command| command.starts_with(prefix))
        .map(str::to_string)
}

#[derive(Debug, Serialize)]
pub struct StatusJson {
    pub schema_version: u32,
    pub branch: Option<String>,
    pub branch_description: Option<String>,
    pub parent: Option<String>,
    pub head: Option<String>,
    pub branch_status: Option<String>,
    pub unmerged_commit_count: usize,
    /// Backward-compatible alias for `working_changes.changes`.
    pub changes: Vec<StatusChangeJson>,
    /// Uncommitted working-tree changes relative to the current commit.
    pub working_changes: StatusChangeSetJson,
    /// Committed contribution relative to the branch's exact fork point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_changes: Option<StatusChangeSetJson>,
    /// Tracked paths whose content the server withheld under path-based
    /// permissions — present in the branch but never materialized locally.
    /// Not deletions; excluded from `changes`. Absent means none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub restricted: Vec<String>,
    pub merge_in_progress: bool,
    pub sync_in_progress: bool,
    pub progress_state: ProgressStateJson,
}

#[derive(Debug, Serialize)]
pub struct StatusChangeSetJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub changes: Vec<StatusChangeJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusCompactJson {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_status: Option<String>,
    pub dirty: bool,
    pub change_count: usize,
    pub change_counts: StatusChangeCountsJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<StatusChangeJson>,
    #[serde(skip_serializing_if = "usize_is_zero")]
    pub changes_omitted: usize,
    pub changes_truncated: bool,
    /// Count of tracked paths withheld by server path permissions (see
    /// `StatusJson::restricted`; run `oak status --json` for the paths).
    #[serde(skip_serializing_if = "usize_is_zero")]
    pub restricted_count: usize,
    #[serde(skip_serializing_if = "bool_is_false")]
    pub merge_in_progress: bool,
    #[serde(skip_serializing_if = "bool_is_false")]
    pub sync_in_progress: bool,
    #[serde(skip_serializing_if = "progress_state_is_idle")]
    pub progress_state: ProgressStateJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recommended_next_commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount: Option<serde_json::Value>,
}

#[derive(Debug, Default, Serialize)]
pub struct StatusChangeCountsJson {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub renamed: usize,
}

#[derive(Debug, Serialize)]
pub struct InfoJson {
    pub schema_version: u32,
    pub branch: Option<String>,
    pub branch_description: Option<String>,
    pub parent: Option<String>,
    pub head: Option<String>,
    pub branch_status: Option<String>,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub remote_url: Option<String>,
    pub merge_in_progress: bool,
    pub sync_in_progress: bool,
    pub progress_state: ProgressStateJson,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct ProgressStateJson {
    pub in_progress: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflict_paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AgentStateJson {
    pub schema_version: u32,
    pub context: String,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub remote_url: Option<String>,
    pub branch: Option<String>,
    pub branch_description: Option<String>,
    pub branch_description_present: bool,
    pub head: Option<String>,
    pub local_parent_head: Option<String>,
    pub remote_parent_head: Option<String>,
    pub remote_parent_fetched_at: Option<String>,
    pub current_branch_pushed_head: Option<String>,
    pub current_branch_push_checked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_branch_push_source: Option<String>,
    pub refresh_requested: bool,
    pub refresh_supported: bool,
    pub refresh_errors: Vec<String>,
    pub needs_pull: bool,
    pub needs_push: bool,
    pub dirty: bool,
    pub changes: Vec<StatusChangeJson>,
    pub unpushed_commit_count: usize,
    pub progress_state: ProgressStateJson,
    pub recommended_next_commands: Vec<String>,
    pub recommended_action: AgentRecommendedActionJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking_reason: Option<String>,
    pub finish_eligible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentStateCompactJson {
    pub schema_version: u32,
    pub context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_description: Option<String>,
    #[serde(skip_serializing_if = "bool_is_false")]
    pub branch_description_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_parent_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_parent_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_parent_fetched_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_branch_pushed_head: Option<String>,
    #[serde(skip_serializing_if = "bool_is_false")]
    pub current_branch_push_checked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_branch_push_source: Option<String>,
    #[serde(skip_serializing_if = "bool_is_false")]
    pub refresh_requested: bool,
    #[serde(skip_serializing_if = "bool_is_true")]
    pub refresh_supported: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refresh_errors: Vec<String>,
    #[serde(skip_serializing_if = "bool_is_false")]
    pub needs_pull: bool,
    #[serde(skip_serializing_if = "bool_is_false")]
    pub needs_push: bool,
    pub finish_eligible: bool,
    pub dirty: bool,
    pub change_count: usize,
    pub change_counts: StatusChangeCountsJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<StatusChangeJson>,
    #[serde(skip_serializing_if = "usize_is_zero")]
    pub changes_omitted: usize,
    pub changes_truncated: bool,
    #[serde(skip_serializing_if = "usize_is_zero")]
    pub unpushed_commit_count: usize,
    pub progress_state: ProgressStateJson,
    pub recommended_next_commands: Vec<String>,
    pub recommended_action: AgentRecommendedActionJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct AgentRecommendedActionJson {
    pub kind: AgentRecommendedActionKindJson,
    pub command: String,
    pub mutates: bool,
    pub needs_network: bool,
    pub confidence: String,
    pub remote_freshness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking_reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risk_notes: Vec<String>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRecommendedActionKindJson {
    Inspect,
    Commit,
    CommitPush,
    Push,
    Finish,
    Pull,
    ResolveConflict,
    CloseBranch,
    ReviewBranch,
    Merge,
    Noop,
}

impl From<AgentStateJson> for AgentStateCompactJson {
    fn from(state: AgentStateJson) -> Self {
        let change_count = state.changes.len();
        let change_counts = status_change_counts(&state.changes);
        let changes_omitted = change_count.saturating_sub(COMPACT_CHANGE_LIMIT);
        let changes_truncated = changes_omitted > 0;
        let mut recommended_next_commands = state.recommended_next_commands;
        if changes_truncated {
            push_unique_command(&mut recommended_next_commands, "oak agent state --json");
            push_unique_command(&mut recommended_next_commands, "oak status --porcelain");
        }
        Self {
            schema_version: crate::work_state::AGENT_STATE_COMPACT_SCHEMA_VERSION,
            context: state.context,
            repo_owner: state.repo_owner,
            repo_name: state.repo_name,
            remote_url: state.remote_url,
            branch: state.branch,
            branch_description: state.branch_description,
            branch_description_present: state.branch_description_present,
            head: state.head,
            local_parent_head: state.local_parent_head,
            remote_parent_head: state.remote_parent_head,
            remote_parent_fetched_at: state.remote_parent_fetched_at,
            current_branch_pushed_head: state.current_branch_pushed_head,
            current_branch_push_checked: state.current_branch_push_checked,
            current_branch_push_source: state.current_branch_push_source,
            refresh_requested: state.refresh_requested,
            refresh_supported: state.refresh_supported,
            refresh_errors: state.refresh_errors,
            needs_pull: state.needs_pull,
            needs_push: state.needs_push,
            finish_eligible: state.finish_eligible,
            dirty: state.dirty,
            change_count,
            change_counts,
            changes: state
                .changes
                .into_iter()
                .take(COMPACT_CHANGE_LIMIT)
                .collect(),
            changes_omitted,
            changes_truncated,
            unpushed_commit_count: state.unpushed_commit_count,
            progress_state: state.progress_state,
            recommended_next_commands,
            recommended_action: state.recommended_action,
            blocking_reason: state.blocking_reason,
            mount: state.mount,
        }
    }
}

pub fn status_change_counts(changes: &[StatusChangeJson]) -> StatusChangeCountsJson {
    let mut counts = StatusChangeCountsJson::default();
    for change in changes {
        match change.status.as_str() {
            "added" => counts.added += 1,
            "modified" => counts.modified += 1,
            "deleted" => counts.deleted += 1,
            "renamed" => counts.renamed += 1,
            _ => {}
        }
    }
    counts
}

fn push_unique_command(commands: &mut Vec<String>, command: &str) {
    if !commands.iter().any(|existing| existing == command) {
        commands.push(command.to_string());
    }
}

fn bool_is_false(value: &bool) -> bool {
    !*value
}

fn bool_is_true(value: &bool) -> bool {
    *value
}

fn usize_is_zero(value: &usize) -> bool {
    *value == 0
}

fn progress_state_is_idle(value: &ProgressStateJson) -> bool {
    !value.in_progress
        && value.kind.is_none()
        && value.conflict_paths.is_empty()
        && value.next_commands.is_empty()
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusChangeJson {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LogCommitJson {
    pub hash: String,
    pub timestamp: String,
    pub branch: String,
    pub description_or_subject: String,
    pub files_changed: usize,
    /// With `-S`/`-G`: the changed paths whose diff matched the pickaxe, so
    /// the reader doesn't pay a follow-up diff per commit to find them.
    /// Append-only — omitted entirely when no pickaxe is active.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pickaxe_matched_paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BranchJson {
    pub schema_version: u32,
    pub name: String,
    pub head: Option<String>,
    pub description: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    pub created_at: String,
    pub current: bool,
}

// ---------------------------------------------------------------------------
// Color gating: never send ANSI escapes to a non-terminal stream
// ---------------------------------------------------------------------------
//
// The decision is made *before* any output is formatted: each `colors`
// constant renders as its escape code only when the destination stream wants
// color, and as nothing otherwise. Gating at render time (rather than
// stripping escapes from the final string) matters for correctness — by the
// time a line is assembled it contains user-controlled content (file
// contents in diffs, paths, branch descriptions), and a post-hoc strip would
// silently mangle data that legitimately contains literal ESC bytes.

/// The color decision for one stream, from the de facto standard env vars:
/// `NO_COLOR` (any non-empty value disables, <https://no-color.org>) wins over
/// `CLICOLOR_FORCE` (non-empty and not "0" forces color even when piped),
/// which wins over plain TTY detection.
fn should_color(no_color: Option<&OsStr>, force: Option<&OsStr>, is_tty: bool) -> bool {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if force.is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    is_tty
}

/// Whether ANSI colors should be written to stdout. Computed once per process.
pub fn stdout_colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        should_color(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("CLICOLOR_FORCE").as_deref(),
            std::io::stdout().is_terminal(),
        )
    })
}

/// Whether ANSI colors should be written to stderr. Computed once per process.
pub fn stderr_colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        should_color(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("CLICOLOR_FORCE").as_deref(),
            std::io::stderr().is_terminal(),
        )
    })
}

/// Flatten one of clap's own stdout messages — subcommand `--help`, `oak help
/// <cmd>`, `--version`, bare `oak mount`'s help — into text for the output
/// funnel, styled or plain according to `colors`.
///
/// clap normally writes these itself, choosing styling from its own TTY probe.
/// `main` intercepts them and re-renders through [`print_raw`] instead, so the
/// broken-pipe contract and the stdout-health check apply — which means the
/// styling decision moves here, and it must be the *same* decision the rest of
/// the CLI makes. Callers pass [`stdout_colors_enabled`], the single stdout
/// color policy (`NO_COLOR` beats `CLICOLOR_FORCE` beats TTY detection);
/// deciding from `is_terminal` alone would make help the one output in the CLI
/// that ignores `NO_COLOR`/`CLICOLOR_FORCE`.
pub fn clap_message_text(rendered: &clap::builder::StyledStr, colors: bool) -> String {
    if colors {
        rendered.ansi().to_string()
    } else {
        rendered.to_string()
    }
}

/// ANSI color codes, gated at render time.
///
/// A [`Color`] used in a format string (the overwhelmingly common case —
/// everything destined for stdout) displays as its escape code only when
/// [`stdout_colors_enabled`]. Either way, a disabled color contributes zero
/// bytes — nothing is ever stripped after formatting.
///
/// **Writing to stderr?** `Display` gates on *stdout*'s decision, which can
/// differ from stderr's (e.g. `oak status | cat` leaves stderr a TTY). Any
/// `eprintln!` must use [`Color::for_stderr`] instead of `{}` formatting —
/// see `vlog` and the upgrade notice in `version_check` for the pattern.
pub mod colors {
    use std::fmt;

    /// One ANSI SGR escape code that knows when to stay silent.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct Color(&'static str);

    impl Color {
        /// The escape code if `enabled`, the empty string otherwise.
        pub fn when(self, enabled: bool) -> &'static str {
            if enabled {
                self.0
            } else {
                ""
            }
        }

        /// The escape code gated on stderr's color decision — for the few
        /// writers that print to stderr (verbose timing, upgrade notice).
        pub fn for_stderr(self) -> &'static str {
            self.when(super::stderr_colors_enabled())
        }
    }

    impl fmt::Display for Color {
        /// Render for **stdout**: the escape code when stdout wants color,
        /// nothing otherwise. Do not rely on this in strings destined for
        /// stderr — use [`Color::for_stderr`] there, since the two streams'
        /// color decisions can differ.
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.when(super::stdout_colors_enabled()))
        }
    }

    pub const RESET: Color = Color("\x1b[0m");
    pub const RED: Color = Color("\x1b[31m");
    pub const GREEN: Color = Color("\x1b[32m");
    pub const YELLOW: Color = Color("\x1b[33m");
    pub const BLUE: Color = Color("\x1b[34m");
    pub const MAGENTA: Color = Color("\x1b[35m");
    pub const CYAN: Color = Color("\x1b[36m");
    pub const WHITE: Color = Color("\x1b[37m");
    pub const BRIGHT_GREEN: Color = Color("\x1b[92m");
    pub const BOLD: Color = Color("\x1b[1m");
    pub const DIM: Color = Color("\x1b[2m");
    pub const UNDERLINE: Color = Color("\x1b[4m");
}

/// Print a success message
pub fn success(msg: &str) {
    emit_line(&format!(
        "{}{}✓{} {}",
        colors::GREEN,
        colors::BOLD,
        colors::RESET,
        msg
    ));
}

/// Print an info message
pub fn info(msg: &str) {
    emit_line(msg);
}

/// Print a warning message (to stderr — diagnostics stay off the data channel)
pub fn warning(msg: &str) {
    emit_stderr_line(&format!(
        "{}{}warning:{} {}",
        colors::YELLOW.for_stderr(),
        colors::BOLD.for_stderr(),
        colors::RESET.for_stderr(),
        msg
    ));
}

/// Print an error message (to stderr — diagnostics stay off the data channel)
pub fn error(msg: &str) {
    emit_stderr_line(&format!(
        "{}{}error:{} {}",
        colors::RED.for_stderr(),
        colors::BOLD.for_stderr(),
        colors::RESET.for_stderr(),
        msg
    ));
}

/// Print a section header (bold + cyan)
pub fn header(msg: &str) {
    emit_line(&format!(
        "{}{}{}{}",
        colors::CYAN,
        colors::BOLD,
        msg,
        colors::RESET,
    ));
}

/// Print an indented detail line (dimmed prefix)
pub fn detail(label: &str, value: &str) {
    emit_line(&format!(
        "  {}{}{} {}",
        colors::DIM,
        label,
        colors::RESET,
        value,
    ));
}

/// Print an indented item (used for file lists, commit lists, etc.)
pub fn item(msg: &str) {
    emit_line(&format!("  {msg}"));
}

/// Print a blank line separator
pub fn blank() {
    emit_line("");
}

/// Format a file status for display
pub fn format_status(status: FileStatus, path: &str) -> String {
    let (color, prefix) = match status {
        FileStatus::Added => (colors::GREEN, "A"),
        FileStatus::Modified => (colors::YELLOW, "M"),
        FileStatus::Deleted => (colors::RED, "D"),
        FileStatus::Unchanged => (colors::DIM, " "),
    };

    format!("{}{}{} {}", color, prefix, colors::RESET, path)
}

/// Format a change type for display
pub fn format_change_type(change_type: ChangeType, path: &str) -> String {
    let (color, prefix) = match change_type {
        ChangeType::Added => (colors::GREEN, "A"),
        ChangeType::Modified => (colors::YELLOW, "M"),
        ChangeType::Deleted => (colors::RED, "D"),
        ChangeType::Renamed => (colors::CYAN, "R"),
    };

    format!("{}{}{} {}", color, prefix, colors::RESET, path)
}

/// Canonical lowercase change name for JSON output.
pub fn change_type_name(change_type: ChangeType) -> &'static str {
    match change_type {
        ChangeType::Added => "added",
        ChangeType::Modified => "modified",
        ChangeType::Deleted => "deleted",
        ChangeType::Renamed => "renamed",
    }
}

/// Format a rename change for display (includes old_path -> new_path)
pub fn format_rename(old_path: &str, new_path: &str) -> String {
    format!(
        "{}R{} {} -> {}",
        colors::CYAN,
        colors::RESET,
        old_path,
        new_path
    )
}

/// Compact count summary for non-TTY status/commit output.
pub fn compact_change_summary(changes: &[FileChange]) -> String {
    let mut added = 0usize;
    let mut modified = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    for change in changes {
        match change.change_type {
            ChangeType::Added => added += 1,
            ChangeType::Modified => modified += 1,
            ChangeType::Deleted => deleted += 1,
            ChangeType::Renamed => renamed += 1,
        }
    }

    let mut parts = Vec::new();
    for (n, label) in [
        (added, "added"),
        (modified, "modified"),
        (deleted, "deleted"),
        (renamed, "renamed"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {label}"));
        }
    }

    format!(
        "{} change{}{}",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" },
        if parts.is_empty() {
            String::new()
        } else {
            format!(": {}", parts.join(", "))
        }
    )
}

/// One compact changed-path row, without prose.
pub fn compact_change_line(change: &FileChange) -> String {
    if change.change_type == ChangeType::Renamed {
        if let Some(old_path) = change.old_path.as_deref() {
            return format!("R {old_path} -> {}", change.path);
        }
    }
    let prefix = match change.change_type {
        ChangeType::Added => "A",
        ChangeType::Modified => "M",
        ChangeType::Deleted => "D",
        ChangeType::Renamed => "R",
    };
    format!("{prefix} {}", change.path)
}

/// Hash prefix length on piped log lines. 8 hex chars comfortably
/// disambiguates within one repository's history (git shows 7 by default)
/// and costs a third less context than the 12-char `Hash::short`.
const LOG_HASH_PREFIX: usize = 8;

/// Changed files listed per commit by the piped `oak log -v` before the
/// remainder is summarized. Log can span many commits, so it keeps a
/// per-commit file sample even though dirty status/diff list every path.
const LOG_VERBOSE_FILE_SAMPLE: usize = 5;

fn log_hash(commit: &Commit) -> &str {
    &commit.hash.0[..LOG_HASH_PREFIX.min(commit.hash.0.len())]
}

/// `<hash8> <date> [(branch)] [<subject>]` — the shared head row of both
/// piped log formats. The branch appears, parenthesized, only when it differs
/// from `current_branch`: piped logs are read by agents, and in single-branch
/// history a branch column repeats the same name on every row without telling
/// them anything.
fn format_commit_row(
    commit: &Commit,
    current_branch: Option<&str>,
    subject: Option<&str>,
) -> String {
    let mut row = format!(
        "{} {}",
        log_hash(commit),
        commit.timestamp.format("%Y-%m-%d")
    );
    if current_branch != Some(commit.branch_name.as_str()) {
        row.push_str(&format!(" ({})", commit.branch_name));
    }
    if let Some(subject) = subject {
        row.push(' ');
        row.push_str(subject);
    }
    row
}

/// One-line commit summary for piped output:
/// `<hash8> <date> [(branch)] <subject>`.
///
/// The subject is the first non-empty line of the commit message; messageless
/// commits (the norm in oak — descriptions live on branches) fall back to the
/// changed-file count so the line still says something useful. Used by the
/// non-TTY `oak log` and the mount-aware log so both read identically.
pub fn format_commit_compact(commit: &Commit, current_branch: Option<&str>) -> String {
    format_commit_row(
        commit,
        current_branch,
        Some(&commit_description_or_subject(commit)),
    )
}

/// Piped verbose log block: the one-line commit row, then one
/// `<letter> <path>` row per changed file. A bounded sample with no
/// indentation or blank separators — every byte of piped output is some
/// agent's context. Messageless commits skip the file-count fallback subject
/// here: the file rows directly below already carry it.
pub fn format_commit_compact_verbose(commit: &Commit, current_branch: Option<&str>) -> String {
    let subject = commit_subject(commit);
    let mut out = match subject {
        Some(ref s) => format_commit_row(commit, current_branch, Some(s)),
        // An empty commit has no file rows to fall back on, so keep the count.
        None if commit.files.is_empty() => format_commit_compact(commit, current_branch),
        None => format_commit_row(commit, current_branch, None),
    };
    for change in commit.files.iter().take(LOG_VERBOSE_FILE_SAMPLE) {
        out.push('\n');
        out.push_str(&compact_change_line(change));
    }
    let rest = commit.files.len().saturating_sub(LOG_VERBOSE_FILE_SAMPLE);
    if rest > 0 {
        out.push_str(&format!("\n... {rest} more"));
    }
    out
}

/// First meaningful message line, if the commit has one.
fn commit_subject(commit: &Commit) -> Option<String> {
    commit
        .message
        .as_deref()
        .and_then(|m| m.lines().find(|l| !l.trim().is_empty()))
        .map(|line| line.trim().to_string())
}

/// First meaningful message line, or a file-count fallback for messageless commits.
pub fn commit_description_or_subject(commit: &Commit) -> String {
    commit_subject(commit).unwrap_or_else(|| {
        let n = commit.files.len();
        format!("{n} file{}", if n == 1 { "" } else { "s" })
    })
}

/// The "there's more history" trailer printed after a truncated piped log.
/// Always tells the reader how to widen the window, so truncation never costs
/// an exploratory extra command.
pub fn format_log_more_hint(shown: usize) -> String {
    format_log_more_hint_with_mode(shown, false, &[], None, None)
}

/// The suggested command reproduces the reader's *whole* query — `--oneline`
/// mode, any `-S`/`-G` pickaxe, and the path filters as originally given —
/// with a doubled `-n`. Dropping a filter here would hand whoever follows
/// the hint a different, unrelated history. Arguments are shell-quoted so
/// the hint is copy-pasteable even when the pattern has regex metacharacters
/// or spaces.
pub fn format_log_more_hint_with_mode(
    shown: usize,
    oneline: bool,
    paths: &[String],
    search: Option<&str>,
    search_regex: Option<&str>,
) -> String {
    let mut cmd = String::from("oak log");
    if oneline {
        cmd.push_str(" --oneline");
    }
    if let Some(term) = search {
        cmd.push_str(" -S ");
        cmd.push_str(&shell_quote(term));
    }
    if let Some(pattern) = search_regex {
        cmd.push_str(" -G ");
        cmd.push_str(&shell_quote(pattern));
    }
    cmd.push_str(&format!(" -n {}", shown.saturating_mul(2).max(1)));
    for path in paths {
        cmd.push(' ');
        cmd.push_str(&shell_quote(path));
    }
    format!("... more commits: {cmd}")
}

/// Quote one argument for POSIX shells: plain-safe strings pass through
/// untouched, anything else is single-quoted (with embedded single quotes
/// spliced as `'\''`).
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@'));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Format a commit for the log display
pub fn format_commit(commit: &Commit, verbose: bool) -> String {
    let mut output = String::new();

    // Commit header
    output.push_str(&format!(
        "{}{}commit {}{}{}\n",
        colors::YELLOW,
        colors::BOLD,
        colors::RESET,
        colors::CYAN,
        commit.hash.short(),
    ));
    output.push_str(&colors::RESET.to_string());
    output.push('\n');

    // Branch
    output.push_str(&format!(
        "{}branch:{} {}{}{}\n",
        colors::DIM,
        colors::RESET,
        colors::BOLD,
        commit.branch_name,
        colors::RESET,
    ));

    // Author and date
    output.push_str(&format!(
        "{}author:{} {}\n",
        colors::DIM,
        colors::RESET,
        commit.author,
    ));
    output.push_str(&format!(
        "{}date:{} {}\n",
        colors::DIM,
        colors::RESET,
        commit.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
    ));

    // Message — only main-branch squash-merge commits have one; for every
    // other commit, the branch (and its description) carry the meaning.
    if let Some(msg) = commit.message.as_deref() {
        output.push_str(&format!("\n    {msg}\n"));
    }

    // File changes (if verbose)
    if verbose && !commit.files.is_empty() {
        output.push_str("\n    files:\n");
        for file in &commit.files {
            if file.change_type == ChangeType::Renamed {
                if let Some(ref old_path) = file.old_path {
                    output.push_str(&format!("      R {} -> {}\n", old_path, file.path));
                } else {
                    output.push_str(&format!("      R {}\n", file.path));
                }
            } else {
                output.push_str(&format!("      {} {}\n", file.change_type, file.path));
            }
        }
    }

    output
}

/// Format a branch for display
pub fn format_branch(br: &Branch, is_current: bool) -> String {
    let mut output = String::new();

    let marker = if is_current { "* " } else { "  " };
    let status_color = match br.status {
        BranchStatus::Open => colors::GREEN,
        BranchStatus::Closed => colors::DIM,
    };

    output.push_str(&format!(
        "{}{}{}{}{}{}\n",
        marker,
        colors::BOLD,
        br.name,
        colors::RESET,
        status_color,
        colors::RESET,
    ));

    if let Some(ref desc) = br.description {
        output.push_str(&format!("    {desc}\n"));
    }

    if let Some(ref parent) = br.parent_branch {
        output.push_str(&format!(
            "    {}parent: {}{}\n",
            colors::DIM,
            parent,
            colors::RESET,
        ));
    }

    output.push_str(&format!(
        "    {}[{}]{}\n",
        status_color,
        br.status,
        colors::RESET,
    ));

    output
}

/// Format diff output with colors.
///
/// Returns a borrow of `line` whenever no color wrapping happens — both in
/// no-color mode and for context lines — so large piped diffs don't pay a
/// per-line allocation just to add nothing.
pub fn format_diff_line(line: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;

    if !stdout_colors_enabled() {
        return Cow::Borrowed(line);
    }
    if line.starts_with('+') && !line.starts_with("+++") {
        Cow::Owned(format!("{}{}{}", colors::GREEN, line, colors::RESET))
    } else if line.starts_with('-') && !line.starts_with("---") {
        Cow::Owned(format!("{}{}{}", colors::RED, line, colors::RESET))
    } else if line.starts_with("@@") {
        Cow::Owned(format!("{}{}{}", colors::CYAN, line, colors::RESET))
    } else if line.starts_with("diff") || line.starts_with("---") || line.starts_with("+++") {
        Cow::Owned(format!("{}{}{}", colors::BOLD, line, colors::RESET))
    } else {
        Cow::Borrowed(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    /// The JSON-payload marker is process-wide, so the tests that exercise
    /// it run one at a time.
    ///
    /// MERGE OBLIGATION (fb-106): this lock and `reset_stdout_markers` must
    /// also be taken by fb-106's
    /// `stdout_failures_silence_writes_and_only_real_errors_surface`, which
    /// asserts `!STDOUT_GONE` at entry — see the doc-comment on
    /// [`mark_json_payload_emitted`].
    static STDOUT_MARKER_LOCK: Mutex<()> = Mutex::new(());

    /// MERGE OBLIGATION (fb-106): must also clear `STDOUT_GONE`.
    fn reset_stdout_markers() {
        STDOUT_GONE.store(false, Ordering::Relaxed);
        JSON_PAYLOAD_EMITTED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn json_payload_marker_is_set_when_the_payload_write_landed() {
        let _guard = STDOUT_MARKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_stdout_markers();
        assert!(!json_payload_emitted());
        // stdout is writable here, so the flush inside the mark succeeds.
        mark_json_payload_emitted();
        assert!(
            json_payload_emitted(),
            "a payload that reached stdout IS this run's JSON document"
        );
        reset_stdout_markers();
    }

    #[test]
    fn json_payload_marker_is_sticky_once_set() {
        // The marker only ever means "this run already put its JSON document
        // on stdout", so nothing later may clear it: a second payload-marking
        // command in the same process must not hand the error envelope back
        // a second document.
        let _guard = STDOUT_MARKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_stdout_markers();
        mark_json_payload_emitted();
        assert!(json_payload_emitted());
        mark_json_payload_emitted();
        assert!(
            json_payload_emitted(),
            "the payload marker must stay set for the life of the process"
        );
        reset_stdout_markers();
    }

    #[test]
    fn json_payload_marker_stays_clear_when_stdout_is_gone() {
        let _guard = STDOUT_MARKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_stdout_markers();
        STDOUT_GONE.store(true, Ordering::Relaxed);
        mark_json_payload_emitted();
        assert!(
            !json_payload_emitted(),
            "a swallowed payload write must not suppress the error envelope"
        );
        reset_stdout_markers();
    }

    #[test]
    fn should_color_follows_tty_by_default() {
        assert!(should_color(None, None, true));
        assert!(!should_color(None, None, false));
    }

    #[test]
    fn no_color_disables_even_on_a_tty() {
        assert!(!should_color(Some(&os("1")), None, true));
        // ...and beats CLICOLOR_FORCE.
        assert!(!should_color(Some(&os("1")), Some(&os("1")), true));
        // An empty NO_COLOR is treated as unset, per the spec.
        assert!(should_color(Some(&os("")), None, true));
    }

    #[test]
    fn clicolor_force_enables_when_piped() {
        assert!(should_color(None, Some(&os("1")), false));
        // "0" and empty mean "not forced".
        assert!(!should_color(None, Some(&os("0")), false));
        assert!(!should_color(None, Some(&os("")), false));
    }

    /// A styled clap message of the shape `main` intercepts (subcommand
    /// `--help`): headings carry clap's default styling, so the ANSI and
    /// plain renderings genuinely differ.
    fn styled_clap_message() -> clap::builder::StyledStr {
        clap::Command::new("demo")
            .about("A demo command")
            .arg(clap::Arg::new("path").help("a path"))
            .render_help()
    }

    /// Intercepted clap help/version output must obey the same stdout color
    /// policy as every other line oak prints — not a bare TTY probe. Composed
    /// with the real decision function so the wiring, not just the branch, is
    /// pinned: `NO_COLOR=1` renders plain even on a terminal, and
    /// `CLICOLOR_FORCE=1` renders ANSI even when stdout is a pipe.
    #[test]
    fn clap_messages_follow_the_stdout_color_policy() {
        let rendered = styled_clap_message();

        let no_color = clap_message_text(&rendered, should_color(Some(&os("1")), None, true));
        assert!(
            !no_color.contains('\u{1b}'),
            "NO_COLOR=1 must render plain help, got: {no_color:?}"
        );
        assert!(no_color.contains("A demo command"), "got: {no_color:?}");

        let forced = clap_message_text(&rendered, should_color(None, Some(&os("1")), false));
        assert!(
            forced.contains('\u{1b}'),
            "CLICOLOR_FORCE=1 must keep clap's styling on a non-TTY stdout, got: {forced:?}"
        );
        assert!(forced.contains("A demo command"), "got: {forced:?}");
    }

    #[test]
    fn color_renders_its_code_only_when_enabled() {
        assert_eq!(colors::GREEN.when(true), "\x1b[32m");
        assert_eq!(colors::GREEN.when(false), "");
        assert_eq!(colors::RESET.when(false), "");
    }

    #[test]
    fn progress_mode_parses_human_env_values() {
        assert_eq!(progress_mode_from_str("never"), ProgressMode::Never);
        assert_eq!(progress_mode_from_str("off"), ProgressMode::Never);
        assert_eq!(progress_mode_from_str("0"), ProgressMode::Never);
        assert_eq!(progress_mode_from_str("always"), ProgressMode::Always);
        assert_eq!(progress_mode_from_str("on"), ProgressMode::Always);
        assert_eq!(progress_mode_from_str("1"), ProgressMode::Always);
        assert_eq!(progress_mode_from_str("surprise"), ProgressMode::Auto);
    }

    #[test]
    fn activity_policy_never_animates_structured_or_captured_output() {
        assert!(!activity_enabled_for(
            true,
            false,
            ProgressMode::Always,
            false,
            true,
            true
        ));
        assert!(!activity_enabled_for(
            false,
            true,
            ProgressMode::Always,
            false,
            true,
            true
        ));
    }

    #[test]
    fn activity_policy_auto_requires_human_terminal_context() {
        assert!(activity_enabled_for(
            false,
            false,
            ProgressMode::Auto,
            false,
            true,
            true
        ));
        assert!(!activity_enabled_for(
            false,
            false,
            ProgressMode::Auto,
            true,
            true,
            true
        ));
        assert!(!activity_enabled_for(
            false,
            false,
            ProgressMode::Auto,
            false,
            false,
            true
        ));
        assert!(!activity_enabled_for(
            false,
            false,
            ProgressMode::Auto,
            false,
            true,
            false
        ));
    }

    #[test]
    fn activity_policy_progress_never_and_always_are_explicit() {
        assert!(!activity_enabled_for(
            false,
            false,
            ProgressMode::Never,
            false,
            true,
            true
        ));
        assert!(activity_enabled_for(
            false,
            false,
            ProgressMode::Always,
            true,
            false,
            false
        ));
    }

    /// Under the test harness stdout is captured (not a TTY), so the
    /// process-wide decision is "no color" and every `Color` must render as
    /// nothing. Skipped if the environment forces color on.
    #[test]
    fn disabled_colors_contribute_zero_bytes_to_formatted_output() {
        if stdout_colors_enabled() {
            return; // CLICOLOR_FORCE set in the environment; nothing to assert
        }
        assert_eq!(
            format_status(FileStatus::Modified, "src/lib.rs"),
            "M src/lib.rs"
        );
        assert_eq!(format_change_type(ChangeType::Added, "a.txt"), "A a.txt");
        assert_eq!(format_rename("old.rs", "new.rs"), "R old.rs -> new.rs");
        assert_eq!(format_diff_line("+added line"), "+added line");
    }

    /// Regression test: color gating must never alter user-controlled
    /// content. A diff line whose *file content* contains literal ANSI
    /// escape bytes has to round-trip byte-for-byte — the old post-hoc
    /// stripping approach corrupted it.
    #[test]
    fn user_content_with_literal_escape_bytes_round_trips_exactly() {
        if stdout_colors_enabled() {
            return;
        }
        let user_line = "+\x1b[31mred\x1b[0m";
        // With colors disabled the wrapper adds nothing — and, crucially,
        // removes nothing.
        assert_eq!(format_diff_line(user_line), user_line);

        begin_capture();
        print_line(user_line);
        assert_eq!(end_capture(), user_line);
    }

    fn test_commit(message: Option<&str>, n_files: usize) -> Commit {
        Commit {
            hash: oak_core::Hash("abcdef123456abcdef123456".to_string()),
            branch_name: "main".to_string(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: oak_core::Hash("0".repeat(24)),
            author: "tester".to_string(),
            message: message.map(str::to_string),
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-06-09T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            files: (0..n_files)
                .map(|i| oak_core::FileChange {
                    path: format!("f{i}.txt"),
                    change_type: ChangeType::Modified,
                    old_path: None,
                    old_blob_hash: None,
                    new_blob_hash: None,
                    old_mode: None,
                    new_mode: None,
                })
                .collect(),
        }
    }

    #[test]
    fn compact_commit_line_uses_message_subject() {
        let c = test_commit(Some("subject line\nbody detail"), 3);
        // On the logged branch the branch column is omitted entirely.
        assert_eq!(
            format_commit_compact(&c, Some("main")),
            "abcdef12 2026-06-09 subject line"
        );
        // Off-branch commits keep the branch, parenthesized, to disambiguate.
        assert_eq!(
            format_commit_compact(&c, Some("feature")),
            "abcdef12 2026-06-09 (main) subject line"
        );
        // Unknown current branch: always show it.
        assert_eq!(
            format_commit_compact(&c, None),
            "abcdef12 2026-06-09 (main) subject line"
        );
    }

    #[test]
    fn compact_commit_line_falls_back_to_file_count_when_messageless() {
        let c = test_commit(None, 2);
        assert!(format_commit_compact(&c, Some("main")).ends_with("2 files"));
        let c = test_commit(None, 1);
        assert!(format_commit_compact(&c, Some("main")).ends_with("1 file"));
        // A whitespace-only message is treated as messageless too.
        let c = test_commit(Some("  \n\n"), 1);
        assert!(format_commit_compact(&c, Some("main")).ends_with("1 file"));
    }

    #[test]
    fn compact_verbose_block_lists_bounded_file_rows() {
        if stdout_colors_enabled() {
            return;
        }
        // Messageless: the file rows carry the change info, so no
        // file-count fallback subject on the head row.
        let c = test_commit(None, 2);
        assert_eq!(
            format_commit_compact_verbose(&c, Some("main")),
            "abcdef12 2026-06-09\nM f0.txt\nM f1.txt"
        );
        // A message subject still appears.
        let c = test_commit(Some("subject"), 1);
        assert_eq!(
            format_commit_compact_verbose(&c, Some("main")),
            "abcdef12 2026-06-09 subject\nM f0.txt"
        );
        // An empty commit has no file rows, so the count fallback stays.
        let c = test_commit(None, 0);
        assert_eq!(
            format_commit_compact_verbose(&c, Some("main")),
            "abcdef12 2026-06-09 0 files"
        );
        // Beyond the sample bound the remainder is summarized, not dropped.
        let c = test_commit(None, 7);
        let block = format_commit_compact_verbose(&c, Some("main"));
        assert!(block.contains("\nM f4.txt\n"), "got: {block}");
        assert!(block.ends_with("... 2 more"), "got: {block}");
        assert!(!block.contains("f5.txt"), "got: {block}");
    }

    #[test]
    fn log_more_hint_doubles_the_window() {
        assert_eq!(format_log_more_hint(20), "... more commits: oak log -n 40");
        assert_eq!(
            format_log_more_hint_with_mode(20, true, &[], None, None),
            "... more commits: oak log --oneline -n 40"
        );
        // Degenerate window still suggests something actionable.
        assert_eq!(format_log_more_hint(0), "... more commits: oak log -n 1");
    }

    /// The hint reproduces the reader's exact query — pickaxe and path
    /// filters included, shell-quoted — never a filterless `oak log -n N`
    /// that would surface unrelated commits.
    #[test]
    fn log_more_hint_preserves_filters_and_quotes_them() {
        let paths = vec!["src/a.txt".to_string(), "docs dir/x.md".to_string()];
        assert_eq!(
            format_log_more_hint_with_mode(20, false, &paths, None, Some(r"fn \w+")),
            r"... more commits: oak log -G 'fn \w+' -n 40 src/a.txt 'docs dir/x.md'"
        );
        assert_eq!(
            format_log_more_hint_with_mode(20, false, &[], Some("it's"), None),
            r"... more commits: oak log -S 'it'\''s' -n 40"
        );
        assert_eq!(
            format_log_more_hint_with_mode(20, true, &["src".to_string()], Some("plain"), None),
            "... more commits: oak log --oneline -S plain -n 40 src"
        );
    }

    #[test]
    fn json_error_envelope_classifies_configuration_errors() {
        let envelope = JsonErrorEnvelope::from_error(&oak_core::OakError::Config(
            "repo is local-only".to_string(),
        ));

        assert_eq!(envelope.error.code, "configuration_error");
        assert_eq!(
            envelope.error.message,
            "Configuration error: repo is local-only"
        );
        assert_eq!(envelope.error.conflict_paths, Vec::<String>::new());
        assert_eq!(
            envelope.error.recommended_next_commands,
            vec!["oak info --json".to_string()]
        );

        let envelope = JsonErrorEnvelope::from_error(&oak_core::OakError::Config(
            format!(
                "This repository is not linked to an Oak remote repo. Run `{}` once to link it before using `oak commit --push`.",
                crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND
            ),
        ));
        assert_eq!(
            envelope.error.recommended_next_commands,
            vec![
                crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string(),
                "oak info --json".to_string()
            ]
        );

        let envelope = JsonErrorEnvelope::from_error(&oak_core::OakError::Config(
            "No Oak credentials found for https://example.test. Run `oak login -r https://example.test` before using `oak commit --push`.".to_string(),
        ));
        assert_eq!(
            envelope.error.recommended_next_commands,
            vec![
                "oak login -r https://example.test".to_string(),
                "oak info --json".to_string()
            ]
        );

        let envelope = JsonErrorEnvelope::from_error(&oak_core::OakError::IncompleteAncestry {
            left: "topic".to_string(),
            right: "main".to_string(),
            missing: "abc123".to_string(),
        });
        assert_eq!(envelope.error.code, "incomplete_ancestry");
        assert_eq!(
            envelope.error.recommended_next_commands,
            vec![
                "oak fetch".to_string(),
                "oak pull --force".to_string(),
                "oak status --json".to_string()
            ]
        );

        let envelope = JsonErrorEnvelope::from_error(&oak_core::OakError::IncompleteManifestData {
            left: "topic".to_string(),
            right: "main".to_string(),
            missing: "def456".to_string(),
        });
        assert_eq!(envelope.error.code, "incomplete_manifest_data");
        assert!(envelope
            .error
            .message
            .contains("Missing manifest(s): def456"));

        let envelope = JsonErrorEnvelope::from_error(&oak_core::OakError::IncompleteCommitData {
            context: "effective HEAD for branch 'topic'".to_string(),
            missing: "abc123".to_string(),
        });
        assert_eq!(envelope.error.code, "incomplete_commit_data");
        assert!(envelope.error.message.contains("Missing commit(s): abc123"));
        assert_eq!(
            envelope.error.recommended_next_commands,
            vec![
                "oak fetch".to_string(),
                "oak pull --force".to_string(),
                "oak status --json".to_string()
            ]
        );

        let envelope =
            JsonErrorEnvelope::from_error(&oak_core::OakError::NoVerifiedCommonAncestor {
                left: "topic".to_string(),
                right: "main".to_string(),
            });
        assert_eq!(envelope.error.code, "no_verified_common_ancestor");
        assert_eq!(
            envelope.error.recommended_next_commands,
            vec![
                "oak fetch".to_string(),
                "oak pull --force".to_string(),
                "oak status --json".to_string()
            ]
        );
    }

    #[test]
    fn captured_output_is_plain_when_colors_are_off() {
        if stdout_colors_enabled() {
            return;
        }
        begin_capture();
        success("done");
        header("Section");
        assert_eq!(end_capture(), "✓ done\nSection");
    }

    /// The stdout-failure state machine, exercised in one test because the
    /// state is process-global and deliberately sticky: a `BrokenPipe` is
    /// "reader hung up" (writes go quiet, the command result stays clean),
    /// while any other write error is recorded and surfaced as an
    /// `OakError::Io` by [`stdout_write_result`].
    #[test]
    fn stdout_failures_silence_writes_and_only_real_errors_surface() {
        use std::io::{Error, ErrorKind};

        let _guard = STDOUT_MARKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_stdout_markers();
        assert!(!STDOUT_GONE.load(Ordering::Relaxed));
        assert!(stdout_write_result().is_ok());

        // Reader hung up: writes stop, but this is not an error.
        note_stdout_error(Error::new(ErrorKind::BrokenPipe, "epipe"));
        assert!(STDOUT_GONE.load(Ordering::Relaxed));
        assert!(stdout_write_result().is_ok());

        // A genuine I/O failure is recorded and mapped to OakError::Io with
        // the original kind preserved.
        note_stdout_error(Error::new(ErrorKind::StorageFull, "disk full"));
        match stdout_write_result() {
            Err(oak_core::OakError::Io(e)) => {
                assert_eq!(e.kind(), ErrorKind::StorageFull);
                assert!(e.to_string().contains("disk full"), "got: {e}");
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}
