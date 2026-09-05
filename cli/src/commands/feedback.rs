//! `oak feedback` (alias: `oak feature-request`) — send feedback or a
//! feature request to the Oak team.
//!
//! Message sources, in priority order: `-m/--message`, `--file` (`-` =
//! stdin), and — only when stdin is an interactive terminal — a git-style
//! `$EDITOR` template flow. Non-interactive callers that provide no message
//! exit 2 immediately so automation never blocks on an editor.
//!
//! Contact email is optional and never required: `--email` → `OAK_EMAIL` →
//! the cached `~/.oak/feedback.json` → a one-time interactive prompt (TTY
//! only, answer cached) → omitted.
//!
//! Exit codes (per the feature-request design, intentionally narrower than
//! the repo-wide contract): 0 success · 1 server/network error · 2 usage
//! error. Errors go to stderr so `--json` keeps stdout parseable.
//!
//! ## Link subcommands
//!
//! `oak feedback link` / `oak feedback unlink` attach a feedback item to the
//! branch (or commit) that addresses it, over the admin-only links API:
//!
//! - `GET    /api/feedback?status=all`         → resolve `fb-N` to an item id
//! - `POST   /api/feedback/{id}/links`         → create a link
//! - `GET    /api/feedback/{id}/links`         → list links (unlink matches here)
//! - `DELETE /api/feedback/{id}/links/{link}`  → remove a link
//!
//! `status=all` is deliberate: the server excludes spam from the default
//! export, and an administrator still has to be able to name — and link — an
//! item that was wrongly marked as spam.
//!
//! Every one of those endpoints answers non-admins with a quiet `404`, so a
//! 404 is reported as "not authorized (or unknown item)" rather than as a
//! bare HTTP status. Bare `oak feedback` (no subcommand) keeps its original
//! submit-only behavior.
//!
//! ## Credential isolation
//!
//! The links API is called with **either** an explicit `OAK_API_KEY` **or**
//! the credential stored for the effective remote — never with the current
//! checkout's repository key. `--remote https://not-your-server` must not be
//! able to make this command hand that server a token minted by the
//! checkout's origin, so when neither source yields a credential the command
//! fails *before* opening a connection. See [`resolve_links_token`].
//!
//! The submit path above is unrelated and unchanged: it is anonymous-capable
//! and attaches a stored login for the remote it is already posting to.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

use super::credentials::{effective_token, get_token_for_server, preferred_author_name};
use crate::output;

const DEFAULT_REMOTE: &str = "https://oak.space";
const EMAIL_UNSET_NOTE: &str = "(not set — pass --email or set OAK_EMAIL)";

/// Flags for `oak feedback`, mirrored from the clap variant in `main.rs`.
pub struct FeedbackOptions {
    pub message: Option<String>,
    pub file: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub remote: Option<String>,
    pub json: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct FeedbackResponse {
    #[serde(default)]
    id: Option<String>,
    /// Human-facing sequential reference (e.g. "fb-1234") assigned by the
    /// server. Quote it when following up — it's the tracking number.
    #[serde(default)]
    r#ref: Option<String>,
    #[serde(default = "default_status")]
    status: String,
}

fn default_status() -> String {
    "ok".to_string()
}

#[derive(Deserialize)]
struct ApiError {
    error: String,
}

/// Cached contact info at `~/.oak/feedback.json`.
#[derive(Serialize, Deserialize, Default)]
struct FeedbackCache {
    #[serde(default)]
    email: Option<String>,
}

/// `oak feedback` — collect a message, resolve identity, POST it to the
/// remote's `/api/feature-requests/cli` endpoint, and report the result.
///
/// Handles its own stderr reporting and exit codes for the paths the design
/// specifies exactly (usage → 2, server/network → 1); anything returned as
/// `Err` is a usage-shaped error that main's generic handler also maps to 2.
pub async fn run(work_path: &Path, opts: FeedbackOptions) -> Result<()> {
    let remote = resolve_remote(work_path, opts.remote.as_deref())?;

    let name = resolve_name(opts.name.as_deref());
    let email = resolve_email(opts.email.as_deref());

    let message = acquire_message(
        opts.message,
        opts.file.as_deref(),
        name.as_deref(),
        email.as_deref(),
    )?;

    let mut body = serde_json::json!({
        "body": message,
        "cli_version": env!("CARGO_PKG_VERSION"),
        "idempotency_key": uuid::Uuid::new_v4().to_string(),
    });
    if let Some(name) = &name {
        body["name"] = serde_json::json!(name);
    }
    if let Some(email) = &email {
        body["email"] = serde_json::json!(email);
    }
    if let Some(title) = opts
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        body["title"] = serde_json::json!(title);
    }

    let client = crate::http::api_client();
    let mut req = client
        .post(format!("{remote}/api/feature-requests/cli"))
        .json(&body);
    // Best-effort attribution: send the login token when we have one for
    // this server. The endpoint works anonymously too.
    if let Some(token) = effective_token(&remote, None) {
        req = req.header("authorization", format!("Bearer {token}"));
    }

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            output::error(&format!("could not reach {remote}: {e}"));
            std::process::exit(1);
        }
    };

    let status = resp.status();
    if status.as_u16() == 429 {
        output::error("You're sending feedback too fast — try again in an hour.");
        std::process::exit(1);
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ApiError>(&text)
            .map(|e| e.error)
            .unwrap_or(text);
        if msg.trim().is_empty() {
            output::error(&format!("server rejected the feedback: HTTP {status}"));
        } else {
            output::error(&format!("server rejected the feedback: {status}: {msg}"));
        }
        std::process::exit(1);
    }

    let parsed: FeedbackResponse = match resp.json().await {
        Ok(parsed) => parsed,
        Err(_) => {
            output::error(
                "could not parse server response: invalid JSON or unexpected response shape",
            );
            std::process::exit(1);
        }
    };

    if opts.json {
        output::print_line(&format!(
            "{}",
            serde_json::json!({ "id": parsed.id, "ref": parsed.r#ref, "status": parsed.status })
        ));
    } else {
        match (&parsed.r#ref, &parsed.id) {
            (Some(r), _) => {
                output::print_line(&format!("↟ feedback sent — filed as {r}. Thank you!"))
            }
            (None, Some(id)) => {
                output::print_line(&format!("↟ feedback sent — thank you! (id: {id})"))
            }
            (None, None) => output::print_line("↟ feedback sent — thank you!"),
        }
    }
    Ok(())
}

/// Resolve the remote to post to: `--remote` flag → `OAK_REMOTE` env → the
/// cwd repo's stored remote (non-fatal if not inside a repo) → oak.space.
fn resolve_remote(work_path: &Path, override_remote: Option<&str>) -> Result<String> {
    if let Some(remote) = override_remote {
        let Some(remote) = super::push::normalize_remote_url(remote) else {
            return Err(OakError::InvalidArgument(
                "remote URL cannot be empty".to_string(),
            ));
        };
        return Ok(remote);
    }
    if let Some(remote) = super::push::env_remote_override() {
        return Ok(remote);
    }
    if let Ok(ctx) = crate::resolve::resolve(work_path) {
        if let Ok(repo) = ctx.open() {
            if let Ok(Some(url)) = repo.get_remote_url() {
                if !url.trim().is_empty() {
                    return Ok(url.trim_end_matches('/').to_string());
                }
            }
        }
    }
    Ok(DEFAULT_REMOTE.to_string())
}

/// Produce the feedback body from `-m`, `--file`, or (interactive terminals
/// only) the `$EDITOR` template flow. Exits directly on the spec'd abort
/// paths: 2 when a non-interactive caller provides no message, 1 when the
/// editor flow yields an empty message.
fn acquire_message(
    message: Option<String>,
    file: Option<&str>,
    name: Option<&str>,
    email: Option<&str>,
) -> Result<String> {
    if let Some(message) = message {
        let message = message.trim().to_string();
        if message.is_empty() {
            return Err(OakError::InvalidArgument(
                "feedback message cannot be empty".to_string(),
            ));
        }
        return Ok(message);
    }
    if let Some(file) = file {
        let raw = if file == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            std::fs::read_to_string(file)?
        };
        let message = raw.trim().to_string();
        if message.is_empty() {
            return Err(OakError::InvalidArgument(format!(
                "feedback message from {} is empty",
                if file == "-" { "stdin" } else { file }
            )));
        }
        return Ok(message);
    }

    if !std::io::stdin().is_terminal() {
        eprintln!("No message provided. Use -m <text> or --file <path>.");
        std::process::exit(2);
    }

    edit_message_interactively(name, email)
}

/// Git-style editor flow: pre-fill a markdown temp file with a commented
/// template, run `$VISUAL` → `$EDITOR` → `vi`, then strip comment lines.
/// An unsuccessful editor exit or an empty result aborts with exit 1.
fn edit_message_interactively(name: Option<&str>, email: Option<&str>) -> Result<String> {
    let temp = tempfile::Builder::new()
        .prefix("oak-feedback-")
        .suffix(".md")
        .tempfile()
        .map_err(OakError::Io)?;
    std::fs::write(temp.path(), render_template(name, email))?;

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    let status = Command::new(&editor)
        .arg(temp.path())
        .status()
        .map_err(|e| OakError::Config(format!("Failed to launch editor '{editor}': {e}")))?;

    if !status.success() {
        eprintln!("Aborting: empty feedback message.");
        std::process::exit(1);
    }

    // Re-read by path: some editors replace the file by rename rather than
    // writing through the original inode.
    let edited = std::fs::read_to_string(temp.path())?;
    let message = strip_comment_lines(&edited);
    if message.is_empty() {
        eprintln!("Aborting: empty feedback message.");
        std::process::exit(1);
    }
    Ok(message)
}

/// The template pre-filled into the editor. First line is left empty for
/// typing; `#` lines are stripped from the result.
fn render_template(name: Option<&str>, email: Option<&str>) -> String {
    format!(
        "\n\
         # Describe the feature you'd like to see in Oak.\n\
         # Lines starting with '#' are ignored. An empty message aborts.\n\
         #\n\
         # Name: {}\n\
         # Email: {}\n",
        name.unwrap_or("(not set)"),
        email.unwrap_or(EMAIL_UNSET_NOTE),
    )
}

/// Drop template comment lines (column-0 `#`, git-style) and trim the rest.
fn strip_comment_lines(input: &str) -> String {
    input
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Name to attribute the feedback to: `--name`, else the same identity local
/// commits use (`OAK_AUTHOR` → logged-in username → OS user). Empty → omitted.
fn resolve_name(flag: Option<&str>) -> Option<String> {
    let name = match flag {
        Some(name) => name.trim().to_string(),
        None => preferred_author_name(""),
    };
    (!name.is_empty()).then_some(name)
}

/// Contact email (always optional): `--email` → `OAK_EMAIL` → cached
/// `~/.oak/feedback.json` → one-time interactive prompt (TTY only; the
/// answer is cached so we never ask twice) → omitted.
fn resolve_email(flag: Option<&str>) -> Option<String> {
    let resolved = choose_email(
        flag,
        std::env::var("OAK_EMAIL").ok().as_deref(),
        load_cached_email().as_deref(),
    );
    if resolved.is_some() {
        return resolved;
    }
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let answer = prompt_for_email()?;
    cache_email(&answer);
    Some(answer)
}

/// Pure precedence: first non-blank of flag → env → cache.
fn choose_email(flag: Option<&str>, env: Option<&str>, cached: Option<&str>) -> Option<String> {
    [flag, env, cached]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

/// Ask once on the terminal; Enter (or any read failure) skips. The prompt
/// goes to stderr so `--json` stdout stays machine-readable.
fn prompt_for_email() -> Option<String> {
    eprint!("Contact email (optional, Enter to skip): ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    let answer = line.trim();
    (!answer.is_empty()).then(|| answer.to_string())
}

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".oak").join("feedback.json"))
}

fn load_cached_email() -> Option<String> {
    let raw = std::fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str::<FeedbackCache>(&raw).ok()?.email
}

/// Best-effort cache write — feedback must never fail because the cache
/// couldn't be persisted. Written 0600: the cached email is contact PII.
fn cache_email(email: &str) {
    let Some(path) = cache_path() else { return };
    let contents = serde_json::json!({ "email": email }).to_string();
    if let Err(e) = crate::atomic_file::write_atomic_private(&path, contents) {
        output::warning(&format!(
            "could not cache contact email at {}: {e}",
            path.display()
        ));
    }
}

// ---------------------------------------------------------------------------
// `oak feedback link` / `oak feedback unlink`
// ---------------------------------------------------------------------------

/// Schema version of the `--json` payloads emitted by `link` / `unlink`.
/// Append-only within this version, per the contract in AGENTS.md.
const LINKS_SCHEMA_VERSION: u32 = 1;

/// Flags for `oak feedback link`, mirrored from the clap variant in `main.rs`.
pub struct FeedbackLinkOptions {
    /// The item to link: `fb-165`, `165`, or a raw feedback id.
    pub item: String,
    /// Branch that addresses the item (defaults to the current branch).
    pub branch: Option<String>,
    /// `<org>/<repo>` (defaults to the current checkout's repo).
    pub repo: Option<String>,
    /// Commit hash to record alongside — or instead of — the branch.
    pub commit: Option<String>,
    pub remote: Option<String>,
    pub json: bool,
}

/// Flags for `oak feedback unlink`, mirrored from the clap variant in `main.rs`.
///
/// The three selectors are mutually exclusive (clap enforces it) and are
/// tried in the order `--link-id` → `--commit` → `--branch`; with none of
/// them, the current checkout's branch is the convenience default.
pub struct FeedbackUnlinkOptions {
    pub item: String,
    /// Exact link id, as printed by `link --json` and by an ambiguous
    /// `unlink`. Identifies the link outright — no matching, no repo filter.
    pub link_id: Option<String>,
    /// Commit hash. Matches commit-only links *and* branch-plus-commit ones,
    /// so a link filed with both is still reachable by its commit.
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub repo: Option<String>,
    pub remote: Option<String>,
    pub json: bool,
}

/// How `unlink` was told to find the link to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnlinkSelector {
    /// `--link-id` — the link's own id; unique by construction.
    LinkId(String),
    /// `--commit` — any link recording this commit, with or without a branch.
    Commit(String),
    /// `--branch` (or the current branch) — links naming this branch.
    Branch(String),
}

/// Why a `link` / `unlink` could not be completed.
///
/// These commands document a narrower exit contract than the repo-wide one
/// (0 success · 1 server/network · 2 usage), so failures are carried as this
/// two-armed type rather than as an [`OakError`] whose code would be mapped
/// by `main`'s generic table. Keeping them as values — instead of exiting
/// from deep inside the call — is also what makes every failure path
/// testable in-process.
#[derive(Debug)]
pub enum FeedbackLinkError {
    /// Bad arguments, or no credential to call the remote with — exit 2.
    Usage(String),
    /// Server, network, authorization, not-found or ambiguity — exit 1.
    Failed(String),
}

impl FeedbackLinkError {
    /// The message the user would see on stderr.
    pub fn message(&self) -> &str {
        match self {
            Self::Usage(msg) | Self::Failed(msg) => msg,
        }
    }

    /// The exit code this failure ends the process with.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Failed(_) => 1,
        }
    }
}

impl std::fmt::Display for FeedbackLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

type LinkResult<T> = std::result::Result<T, FeedbackLinkError>;

fn usage<T: std::fmt::Display>(msg: T) -> FeedbackLinkError {
    FeedbackLinkError::Usage(msg.to_string())
}

fn failed<T: std::fmt::Display>(msg: T) -> FeedbackLinkError {
    FeedbackLinkError::Failed(msg.to_string())
}

/// Trim a flag value and drop it if it was blank — `--branch ""` names no
/// branch, and must never be mistaken for one.
fn present(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// One feedback→branch/commit link as the server returns it. Parsed
/// leniently (every field defaulted, unknown fields preserved in `extra`)
/// so a server-side field addition passes straight through to `--json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FeedbackLink {
    #[serde(default)]
    id: serde_json::Value,
    #[serde(default)]
    feedback_id: serde_json::Value,
    #[serde(default)]
    repo_owner: String,
    #[serde(default)]
    repo_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit_hash: Option<String>,
    #[serde(default)]
    link_type: String,
    /// How the link came to exist: `manual`, `branch_description`, `merge`.
    /// Added server-side after this command shipped, so it is optional on
    /// the way in and omitted from `--json` when the server didn't send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl FeedbackLink {
    /// The link's id rendered for a URL path — the server may hand back a
    /// string id or a numeric one, and both address the same row.
    fn id_str(&self) -> Option<String> {
        scalar_str(&self.id)
    }

    /// A one-line description used when several links match an unlink.
    /// Starts with the link id because that id is what the follow-up
    /// `--link-id` command needs.
    fn summary(&self) -> String {
        let target = match (&self.branch, &self.commit_hash) {
            (Some(b), Some(c)) if !b.is_empty() && !c.is_empty() => format!("{b} @ {c}"),
            (Some(b), _) if !b.is_empty() => b.clone(),
            (_, Some(c)) if !c.is_empty() => c.clone(),
            _ => "(no branch or commit)".to_string(),
        };
        let id = self.id_str().unwrap_or_else(|| "?".to_string());
        let source = match self
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(source) => format!(" (source: {source})"),
            None => String::new(),
        };
        format!(
            "{id}  {}/{}  {target}  [{}]{source}",
            self.repo_owner, self.repo_name, self.link_type
        )
    }

    fn branch_str(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
    }

    fn commit_str(&self) -> Option<&str> {
        self.commit_hash
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }
}

/// One row of `GET /api/feedback`. Only the fields `fb-N` resolution needs
/// are named; the rest (including the `links` array) is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
struct FeedbackItem {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    r#ref: Option<String>,
}

/// How the user named the feedback item on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemRef {
    /// `fb-165` or `165` — the human-facing `number`, resolved via the list.
    Number(u64),
    /// A raw feedback id, usable in the URL path as-is.
    Id(String),
}

/// A feedback item resolved to the id the links API wants, plus the label
/// to echo back at the user (`fb-165` when we know the number).
struct ResolvedItem {
    id: String,
    label: String,
    r#ref: Option<String>,
}

/// String form of a JSON scalar, for ids the server may send as either a
/// string or a number. Anything else (null, object, array) has no id.
fn scalar_str(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `oak feedback link <fb-N> --branch <name>` — attach the item to the
/// branch (or commit) that addresses it.
///
/// Thin wrapper over [`try_link`]: the outcome is reported the way this
/// command documents it (message on stderr so `--json` stdout stays
/// parseable; exit 1 for server/network, 2 for usage).
pub async fn link(work_path: &Path, opts: FeedbackLinkOptions) -> Result<()> {
    report(try_link(work_path, opts).await)
}

/// `oak feedback unlink <fb-N>` — drop a link. See [`try_unlink`].
pub async fn unlink(work_path: &Path, opts: FeedbackUnlinkOptions) -> Result<()> {
    report(try_unlink(work_path, opts).await)
}

/// Turn a link/unlink outcome into the process's fate. Usage errors go back
/// through `main`'s generic handler, which already maps `InvalidArgument` to
/// exit 2; everything else prints and exits 1 directly, because the
/// repo-wide table would map a server error to 6 and this command promises 1.
fn report(outcome: LinkResult<()>) -> Result<()> {
    match outcome {
        Ok(()) => Ok(()),
        Err(FeedbackLinkError::Usage(msg)) => Err(OakError::InvalidArgument(msg)),
        Err(FeedbackLinkError::Failed(msg)) => {
            output::error(&msg);
            output::exit_process(1)
        }
    }
}

/// The whole of `oak feedback link`, as a value-returning function so every
/// failure path is reachable from a test instead of ending the process.
pub async fn try_link(work_path: &Path, opts: FeedbackLinkOptions) -> LinkResult<()> {
    let remote = resolve_remote(work_path, opts.remote.as_deref()).map_err(usage)?;
    let item_ref = parse_item_ref(&opts.item)?;
    let (repo_owner, repo_name) = resolve_repo(work_path, opts.repo.as_deref())?;
    let branch = resolve_link_branch(work_path, opts.branch.as_deref(), opts.commit.as_deref())?;
    let commit = present(opts.commit.as_deref());

    // Credential check first: nothing is sent to `remote` until we know we
    // hold a credential that is actually *for* `remote`.
    let api = LinksApi::new(remote)?;
    let item = api.resolve_item(&item_ref).await?;

    let body = link_request_body(
        &repo_owner,
        &repo_name,
        branch.as_deref(),
        commit.as_deref(),
    );
    let created = api.create_link(&item.id, &body).await?;

    if opts.json {
        output::print_json(&LinkJson {
            schema_version: LINKS_SCHEMA_VERSION,
            status: "ok",
            feedback_id: &item.id,
            feedback_ref: item.r#ref.as_deref(),
            link: &created,
            recommended_next_commands: vec![at_remote(
                unlink_hint(&item.label, &created),
                &api.remote,
            )],
        })
        .map_err(failed)?;
    } else {
        output::print_line(&format!(
            "↟ {} linked to {}.",
            item.label,
            link_target(
                &repo_owner,
                &repo_name,
                branch.as_deref(),
                commit.as_deref()
            )
        ));
    }
    Ok(())
}

/// The whole of `oak feedback unlink`.
///
/// `--link-id` names the row outright; `--commit` and `--branch` are
/// resolved by listing the item's links and matching. An ambiguous match is
/// never guessed at — it comes back as a failure that names every candidate
/// link id together with the exact `--link-id` command that resolves it.
pub async fn try_unlink(work_path: &Path, opts: FeedbackUnlinkOptions) -> LinkResult<()> {
    let remote = resolve_remote(work_path, opts.remote.as_deref()).map_err(usage)?;
    let item_ref = parse_item_ref(&opts.item)?;
    let selector = resolve_unlink_selector(work_path, &opts)?;
    // A link id is unique on its own, so `--repo` neither narrows it nor is
    // required — which is what makes `--link-id` work outside a checkout.
    let repo = match selector {
        UnlinkSelector::LinkId(_) => None,
        _ => Some(resolve_repo(work_path, opts.repo.as_deref())?),
    };

    let api = LinksApi::new(remote)?;
    let item = api.resolve_item(&item_ref).await?;
    let links = api.list_links(&item.id).await?;

    let removed = choose_link(
        &links,
        &selector,
        repo.as_ref(),
        &item.label,
        Some(&api.remote),
    )?
    .clone();
    let target = selector_target(&selector, repo.as_ref());
    let Some(link_id) = removed.id_str() else {
        return Err(failed(format!(
            "server returned a link for {target} with no id — cannot delete it"
        )));
    };
    api.delete_link(&item.id, &link_id).await?;

    if opts.json {
        output::print_json(&UnlinkJson {
            schema_version: LINKS_SCHEMA_VERSION,
            status: "ok",
            feedback_id: &item.id,
            feedback_ref: item.r#ref.as_deref(),
            removed_link_id: &link_id,
            removed: &removed,
            // Echo the link as the server spelled it, not as the caller
            // happened to type it.
            recommended_next_commands: vec![at_remote(
                relink_hint(&item.label, &removed),
                &api.remote,
            )],
        })
        .map_err(failed)?;
    } else {
        output::print_line(&format!("↟ {} unlinked from {target}.", item.label));
    }
    Ok(())
}

/// Which link `unlink` was asked for: `--link-id` → `--commit` → `--branch`
/// → the current checkout's branch. clap rejects more than one selector, so
/// the order here only decides the default, never a precedence fight.
fn resolve_unlink_selector(
    work_path: &Path,
    opts: &FeedbackUnlinkOptions,
) -> LinkResult<UnlinkSelector> {
    if let Some(id) = present(opts.link_id.as_deref()) {
        return Ok(UnlinkSelector::LinkId(id));
    }
    if let Some(commit) = present(opts.commit.as_deref()) {
        return Ok(UnlinkSelector::Commit(commit));
    }
    if let Some(branch) = present(opts.branch.as_deref()) {
        return Ok(UnlinkSelector::Branch(branch));
    }
    match resolve_branch(work_path, None) {
        Some(branch) => Ok(UnlinkSelector::Branch(branch)),
        None => Err(usage(
            "nothing to unlink — pass --link-id <id>, --commit <hash> or --branch <name> (there is no current branch here)",
        )),
    }
}

/// Pick the one link a selector names, or explain precisely why it can't.
///
/// The two failure shapes are deliberately different: "no match" lists what
/// the item *is* linked to (so a wrong `--branch`/`--repo` is obvious), while
/// "several matched" lists the candidates' link ids **and** the exact
/// commands that disambiguate them, so the user's next step is copy-paste
/// rather than guesswork.
fn choose_link<'a>(
    links: &'a [FeedbackLink],
    selector: &UnlinkSelector,
    repo: Option<&(String, String)>,
    label: &str,
    remote: Option<&str>,
) -> LinkResult<&'a FeedbackLink> {
    let matches = matching_links(links, selector, repo);
    let target = selector_target(selector, repo);
    match matches.as_slice() {
        [] => Err(failed(format!(
            "{label} has no link to {target} — nothing to unlink.{}",
            other_links_hint(links)
        ))),
        [only] => Ok(only),
        many => Err(failed(ambiguous_message(label, &target, many, remote))),
    }
}

/// The "several matched" message: the candidates, then runnable commands.
fn ambiguous_message(
    label: &str,
    target: &str,
    many: &[&FeedbackLink],
    remote: Option<&str>,
) -> String {
    let mut msg = format!(
        "{label} has {} links to {target} — refusing to guess which one to remove:",
        many.len()
    );
    for link in many {
        msg.push_str(&format!("\n    {}", link.summary()));
    }
    let commands: Vec<String> = many
        .iter()
        .filter_map(|link| link.id_str())
        .map(|id| {
            let command = feedback_command(&["unlink", label, "--link-id", &id]);
            let command = match remote {
                Some(remote) => at_remote(command, remote),
                None => command,
            };
            format!("    {command}")
        })
        .collect();
    if commands.is_empty() {
        // Nothing carried an id, so no `--link-id` command can be written.
        msg.push_str("\nNone of them came back with an id, so they cannot be addressed individually — this is a server bug.");
    } else {
        msg.push_str("\nRe-run naming the one you mean:\n");
        msg.push_str(&commands.join("\n"));
    }
    msg
}

/// How a selector reads in a message.
fn selector_target(selector: &UnlinkSelector, repo: Option<&(String, String)>) -> String {
    let (owner, name) = match repo {
        Some((owner, name)) => (owner.as_str(), name.as_str()),
        None => match selector {
            UnlinkSelector::LinkId(id) => return format!("link {id}"),
            _ => ("?", "?"),
        },
    };
    match selector {
        UnlinkSelector::LinkId(id) => format!("link {id}"),
        UnlinkSelector::Commit(commit) => link_target(owner, name, None, Some(commit)),
        UnlinkSelector::Branch(branch) => link_target(owner, name, Some(branch), None),
    }
}

/// `<owner>/<repo>@<branch>` — or `<owner>/<repo> commit <hash>` when the
/// link names a commit only. Used in both human output and error messages.
fn link_target(owner: &str, name: &str, branch: Option<&str>, commit: Option<&str>) -> String {
    match (branch, commit) {
        (Some(branch), Some(commit)) => format!("{owner}/{name}@{branch} ({commit})"),
        (Some(branch), None) => format!("{owner}/{name}@{branch}"),
        (None, Some(commit)) => format!("{owner}/{name} commit {commit}"),
        (None, None) => format!("{owner}/{name}"),
    }
}

/// Tail for the "no such link" error: name the links the item *does* have,
/// so a wrong `--branch`/`--repo` is obvious without a second command.
fn other_links_hint(links: &[FeedbackLink]) -> String {
    if links.is_empty() {
        return String::new();
    }
    let mut out = String::from(" It is linked to:");
    for link in links {
        out.push_str(&format!("\n    {}", link.summary()));
    }
    out
}

/// The exact `oak feedback unlink` invocation that undoes a link, for
/// `recommended_next_commands`.
///
/// Always `--link-id` when the server sent an id: that form is unambiguous,
/// needs no checkout, and works verbatim even when the item has several
/// links to the same branch. The repo/branch form is only a fallback for a
/// server that returned a link with no id at all.
fn unlink_hint(label: &str, link: &FeedbackLink) -> String {
    if let Some(id) = link.id_str() {
        return feedback_command(&["unlink", label, "--link-id", &id]);
    }
    let repo = format!("{}/{}", link.repo_owner, link.repo_name);
    match (link.branch_str(), link.commit_str()) {
        (Some(branch), _) => {
            feedback_command(&["unlink", label, "--repo", &repo, "--branch", branch])
        }
        (None, Some(commit)) => {
            feedback_command(&["unlink", label, "--repo", &repo, "--commit", commit])
        }
        (None, None) => feedback_command(&["unlink", label, "--repo", &repo]),
    }
}

/// One rendering boundary for all arguments, including response-derived
/// item/link identities and repo/branch/commit values. Data never becomes
/// shell syntax; literal newlines remain inside a single quoted argument.
fn shell_argument(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn feedback_command(arguments: &[&str]) -> String {
    std::iter::once("oak")
        .chain(std::iter::once("feedback"))
        .chain(arguments.iter().copied())
        .map(shell_argument)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Retry/undo commands must stay on the same API origin even when a caller
/// overrode the checkout remote. Only the already normalized, secret-free
/// remote is rendered; metacharacters in a configured base path are quoted.
fn at_remote(command: String, remote: &str) -> String {
    format!("{command} --remote {}", shell_argument(remote))
}

/// The `oak feedback link` invocation that restores a link `unlink` just
/// removed, spelled with the repo/branch/commit the *server* recorded.
fn relink_hint(label: &str, link: &FeedbackLink) -> String {
    let repo = format!("{}/{}", link.repo_owner, link.repo_name);
    let mut args = vec!["link", label, "--repo", &repo];
    if let Some(branch) = link.branch_str() {
        args.extend(["--branch", branch]);
    }
    if let Some(commit) = link.commit_str() {
        args.extend(["--commit", commit]);
    }
    feedback_command(&args)
}

/// Read `fb-165`, `165`, or a raw feedback id. Digits (with or without the
/// `fb-` prefix) are the human-facing `number`; a bare hex string is the
/// item's own id.
fn parse_item_ref(raw: &str) -> LinkResult<ItemRef> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let digits = lower.strip_prefix("fb-").unwrap_or(lower.as_str());
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        return digits
            .parse::<u64>()
            .map(ItemRef::Number)
            .map_err(|_| invalid_item(trimmed));
    }
    // Feedback ids are 28 hex characters; accept any plausible hex id so a
    // future id length keeps working.
    if trimmed.len() >= 8 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(ItemRef::Id(trimmed.to_string()));
    }
    Err(invalid_item(trimmed))
}

fn invalid_item(raw: &str) -> FeedbackLinkError {
    usage(format!(
        "could not read '{raw}' as a feedback item — use fb-165, 165, or the item's id"
    ))
}

/// `<org>/<repo>` from `--repo`, else the current checkout's repo identity.
fn resolve_repo(work_path: &Path, flag: Option<&str>) -> LinkResult<(String, String)> {
    if let Some(flag) = flag {
        return parse_repo_slug(flag);
    }
    let ctx = crate::resolve::resolve(work_path)
        .map_err(|_| usage("not inside an Oak checkout — pass --repo <org>/<repo>"))?;
    let repo = ctx.open().map_err(usage)?;
    super::read_repo_identity(repo.as_ref()).map_err(usage)
}

/// Split `<org>/<repo>`, rejecting anything else so a typo never reaches
/// the server as a silently wrong link.
fn parse_repo_slug(slug: &str) -> LinkResult<(String, String)> {
    let slug = slug.trim().trim_end_matches('/');
    let mut parts = slug.splitn(2, '/');
    let owner = parts.next().unwrap_or_default().trim();
    let name = parts.next().unwrap_or_default().trim();
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(usage(format!(
            "--repo must look like <org>/<repo> (got '{slug}')"
        )));
    }
    Ok((owner.to_string(), name.to_string()))
}

/// The branch to record: `--branch`, else the current branch — but *only*
/// when no `--commit` was given, so `--commit` alone stays a commit link.
fn resolve_link_branch(
    work_path: &Path,
    flag: Option<&str>,
    commit: Option<&str>,
) -> LinkResult<Option<String>> {
    if let Some(branch) = present(flag) {
        return Ok(Some(branch));
    }
    if present(commit).is_some() {
        return Ok(None);
    }
    resolve_branch(work_path, None).map(Some).ok_or_else(|| {
        usage(
            "nothing to link — pass --branch <name> or --commit <hash> (there is no current branch here)",
        )
    })
}

/// `--branch` if given, else the current checkout's branch (absent when the
/// command runs outside a checkout).
fn resolve_branch(work_path: &Path, flag: Option<&str>) -> Option<String> {
    if let Some(branch) = flag.map(str::trim).filter(|b| !b.is_empty()) {
        return Some(branch.to_string());
    }
    let ctx = crate::resolve::resolve(work_path).ok()?;
    let repo = ctx.open().ok()?;
    repo.get_current_branch_name().ok().flatten()
}

/// The `POST /api/feedback/{id}/links` body. `link_type` is `branch`
/// whenever a branch is named, and `commit` for a commit-only link.
fn link_request_body(
    repo_owner: &str,
    repo_name: &str,
    branch: Option<&str>,
    commit: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "repo_owner": repo_owner,
        "repo_name": repo_name,
        "link_type": if branch.is_some() { "branch" } else { "commit" },
    });
    if let Some(branch) = branch {
        body["branch"] = serde_json::json!(branch);
    }
    if let Some(commit) = commit {
        body["commit_hash"] = serde_json::json!(commit);
    }
    body
}

/// The links a selector matches.
///
/// - `--link-id` matches on the id alone, ignoring `repo` entirely.
/// - `--branch` matches links naming that branch.
/// - `--commit` matches links recording that commit *whether or not* they
///   also name a branch — a `branch`-type link filed with `--commit` is
///   otherwise unreachable by its hash.
///
/// Repo owner/name and commit hashes are compared case-insensitively (the
/// server stores repo names as typed; hashes are hex). Branch names are
/// compared exactly — Oak branch names are case-sensitive.
fn matching_links<'a>(
    links: &'a [FeedbackLink],
    selector: &UnlinkSelector,
    repo: Option<&(String, String)>,
) -> Vec<&'a FeedbackLink> {
    let in_repo = |link: &FeedbackLink| match repo {
        Some((owner, name)) => {
            link.repo_owner.eq_ignore_ascii_case(owner) && link.repo_name.eq_ignore_ascii_case(name)
        }
        None => true,
    };
    links
        .iter()
        .filter(|link| match selector {
            UnlinkSelector::LinkId(id) => link.id_str().as_deref() == Some(id.as_str()),
            UnlinkSelector::Branch(branch) => {
                in_repo(link) && link.branch_str() == Some(branch.as_str())
            }
            UnlinkSelector::Commit(commit) => {
                in_repo(link)
                    && link
                        .commit_str()
                        .is_some_and(|c| c.eq_ignore_ascii_case(commit))
            }
        })
        .collect()
}

/// Find the item a `fb-N` reference names.
fn find_by_number(items: &[FeedbackItem], number: u64) -> Option<&FeedbackItem> {
    items.iter().find(|item| item.number == Some(number))
}

// --- HTTP ------------------------------------------------------------------

/// The Bearer token to call `remote` with — and *only* `remote`.
///
/// Exactly two sources, in this order:
///
/// 1. `OAK_API_KEY`, an explicit instruction from the caller.
/// 2. The credential stored for `remote` itself, i.e. what `oak login
///    --remote <remote>` saved.
///
/// There is deliberately no repository-key fallback. `oak feedback link`
/// takes a `--remote`, so such a fallback would mean
/// `--remote https://someone-elses-server` sends that server a token minted
/// by *this* checkout's origin. Returning `None` here — and failing before a
/// connection is opened — is what makes that impossible.
fn resolve_links_token(remote: &str) -> Option<String> {
    choose_links_token(
        std::env::var("OAK_API_KEY").ok().as_deref(),
        get_token_for_server(remote).as_deref(),
    )
}

/// Pure precedence, so the rule itself is testable without touching the
/// environment: explicit env key → the credential stored for the remote
/// being called → nothing. Blank values are not credentials.
fn choose_links_token(env_key: Option<&str>, stored_for_remote: Option<&str>) -> Option<String> {
    present(env_key).or_else(|| present(stored_for_remote))
}

/// The error raised when neither credential source yields a token. Raised
/// before any connection is opened, so nothing is disclosed to `remote`.
fn missing_credential(remote: &str) -> FeedbackLinkError {
    usage(format!(
        "no credential for {remote} — run `oak login --remote {remote}` first, or set OAK_API_KEY (feedback links are admin-only).\nA credential stored for a different server is never sent to {remote}."
    ))
}

/// The admin-only feedback-links API for one remote. Every call carries the
/// Bearer token [`resolve_links_token`] found *for that remote*; without one
/// the endpoints are indistinguishable from "does not exist", so a missing
/// credential is caught before any request is made.
struct LinksApi {
    remote: String,
    token: String,
}

impl LinksApi {
    fn new(remote: String) -> LinkResult<Self> {
        // This boundary also covers legacy stored remotes. Do not normalize
        // only explicit flags: raw userinfo/query/fragment must never become
        // credential lookup keys, request URLs, or public diagnostics.
        let remote = super::push::normalize_remote_url(&remote)
            .ok_or_else(|| usage("feedback links require a valid HTTP(S) remote URL"))?;
        let Some(token) = resolve_links_token(&remote) else {
            return Err(missing_credential(&remote));
        };
        Ok(Self { remote, token })
    }

    fn get(&self, url: String) -> reqwest::RequestBuilder {
        self.authed(crate::http::api_client().get(url))
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("authorization", format!("Bearer {}", self.token))
    }

    /// Turn `fb-165` / `165` into the id the links endpoints take. A raw id
    /// is used as-is, so the common case costs no extra round trip.
    ///
    /// The lookup asks for `?status=all` on purpose: the default export
    /// hides spam-marked items, and an admin still has to be able to reach
    /// one — usually precisely because it was marked as spam by mistake.
    async fn resolve_item(&self, item: &ItemRef) -> LinkResult<ResolvedItem> {
        let number = match item {
            ItemRef::Id(id) => {
                return Ok(ResolvedItem {
                    id: id.clone(),
                    label: id.clone(),
                    r#ref: None,
                });
            }
            ItemRef::Number(number) => *number,
        };

        let items: Vec<FeedbackItem> = self
            .send_json(
                self.get(format!("{}/api/feedback?status=all", self.remote)),
                "list feedback",
            )
            .await?;
        let found = find_by_number(&items, number).ok_or_else(|| {
            failed(format!("no feedback item fb-{number} on {} — check the number, or you may not have access (the feedback API is admin-only)", self.remote))
        })?;
        let id = found
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| failed(format!("server returned fb-{number} without an id")))?;
        Ok(ResolvedItem {
            id: id.to_string(),
            label: found
                .r#ref
                .clone()
                .unwrap_or_else(|| format!("fb-{number}")),
            r#ref: found.r#ref.clone(),
        })
    }

    /// `POST /api/feedback/{id}/links`.
    async fn create_link(&self, id: &str, body: &serde_json::Value) -> LinkResult<FeedbackLink> {
        let req = self
            .authed(crate::http::api_client().post(self.links_url(id)))
            .json(body);
        self.send_json(req, "link this feedback item").await
    }

    /// `GET /api/feedback/{id}/links`.
    async fn list_links(&self, id: &str) -> LinkResult<Vec<FeedbackLink>> {
        self.send_json(self.get(self.links_url(id)), "list this item's links")
            .await
    }

    /// `DELETE /api/feedback/{id}/links/{link_id}`.
    async fn delete_link(&self, id: &str, link_id: &str) -> LinkResult<()> {
        let req = self
            .authed(crate::http::api_client().delete(format!("{}/{link_id}", self.links_url(id))));
        let resp = self.send(req).await?;
        self.check_status(resp, "unlink this feedback item")
            .await
            .map(|_| ())
    }

    fn links_url(&self, id: &str) -> String {
        format!("{}/api/feedback/{id}/links", self.remote)
    }

    /// Send, check the status, and parse the body — the shape every call but
    /// `DELETE` needs.
    async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> LinkResult<T> {
        let resp = self.send(req).await?;
        let body = self.check_status(resp, what).await?;
        serde_json::from_str(&body).map_err(|_| {
            failed(format!(
                "could not parse the server's response ({what}): invalid JSON or unexpected response shape"
            ))
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> LinkResult<reqwest::Response> {
        req.send().await.map_err(|e| {
            failed(format!(
                "could not reach {}: {}",
                self.remote,
                e.without_url()
            ))
        })
    }

    /// Map a non-2xx response onto the message the user should see. The
    /// links API is admin-gated and answers everyone else with a quiet 404,
    /// so that status gets the "not authorized (or unknown item)" wording
    /// rather than a bare HTTP code.
    async fn check_status(&self, resp: reqwest::Response, what: &str) -> LinkResult<String> {
        let status = resp.status();
        if what == "list feedback" && matches!(status.as_u16(), 400 | 422) {
            return Err(failed(
                "server rejected status=all lookup; fb-N resolution requires the updated feedback API. No link was changed. Upgrade the server, or use a raw item ID if its links API is available; refusing an incomplete default-list fallback.",
            ));
        }
        if status.is_redirection() {
            return Err(failed(format!(
                "could not {what}: HTTP {status}; feedback link requests do not follow redirects. Select the intended remote and log in there explicitly."
            )));
        }
        if status.as_u16() == 404 {
            return Err(failed(format!(
                "not authorized (or unknown item) — could not {what} on {}. The feedback API is admin-only and answers everyone else with 404.",
                self.remote
            )));
        }
        if status.as_u16() == 429 {
            return Err(failed(
                "You're sending requests too fast — try again in a bit.",
            ));
        }
        if !status.is_success() {
            // Redact before excerpting: truncating a long echoed credential
            // first would prevent an exact-token replacement from matching.
            let detail = resp.text().await.unwrap_or_default();
            let detail = detail.replace(&self.token, "[redacted]");
            let excerpt: String = detail.trim().chars().take(512).collect();
            return Err(failed(format!(
                "could not {what}: HTTP {status}: {excerpt}"
            )));
        }
        resp.text().await.map_err(|e| {
            failed(format!(
                "could not read the server's response ({what}): {e}"
            ))
        })
    }
}

// --- `--json` envelopes (append-only per the AGENTS.md schema policy) -------

#[derive(Serialize)]
struct LinkJson<'a> {
    schema_version: u32,
    status: &'a str,
    feedback_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    feedback_ref: Option<&'a str>,
    link: &'a FeedbackLink,
    recommended_next_commands: Vec<String>,
}

#[derive(Serialize)]
struct UnlinkJson<'a> {
    schema_version: u32,
    status: &'a str,
    feedback_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    feedback_ref: Option<&'a str>,
    removed_link_id: &'a str,
    removed: &'a FeedbackLink,
    recommended_next_commands: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_comment_lines_drops_template_and_trims() {
        let edited = "\nPlease add dark mode.\n\nIt burns.\n\
                      # Describe the feature you'd like to see in Oak.\n\
                      # Lines starting with '#' are ignored. An empty message aborts.\n\
                      #\n\
                      # Name: mrmrs\n\
                      # Email: adam@oak.space\n";
        assert_eq!(
            strip_comment_lines(edited),
            "Please add dark mode.\n\nIt burns."
        );
    }

    #[test]
    fn strip_comment_lines_keeps_indented_hashes_but_drops_column_zero() {
        assert_eq!(
            strip_comment_lines("code:\n  # indented stays\n# comment goes\n"),
            "code:\n  # indented stays"
        );
    }

    #[test]
    fn strip_comment_lines_untouched_template_is_empty() {
        let untouched = render_template(Some("mrmrs"), None);
        assert_eq!(strip_comment_lines(&untouched), "");
        assert_eq!(strip_comment_lines("   \n\t\n"), "");
        assert_eq!(strip_comment_lines(""), "");
    }

    #[test]
    fn choose_email_precedence_flag_env_cache() {
        assert_eq!(
            choose_email(Some("flag@x.io"), Some("env@x.io"), Some("cache@x.io")),
            Some("flag@x.io".to_string())
        );
        assert_eq!(
            choose_email(None, Some("env@x.io"), Some("cache@x.io")),
            Some("env@x.io".to_string())
        );
        assert_eq!(
            choose_email(None, None, Some("cache@x.io")),
            Some("cache@x.io".to_string())
        );
        assert_eq!(choose_email(None, None, None), None);
    }

    #[test]
    fn choose_email_skips_blank_values() {
        assert_eq!(
            choose_email(Some("  "), Some(""), Some("cache@x.io")),
            Some("cache@x.io".to_string())
        );
        assert_eq!(
            choose_email(Some(" padded@x.io "), None, None),
            Some("padded@x.io".to_string())
        );
    }

    #[test]
    fn render_template_prefills_identity_and_starts_with_blank_line() {
        let t = render_template(Some("mrmrs"), Some("adam@oak.space"));
        assert!(t.starts_with('\n'), "first line must be empty for typing");
        assert!(t.contains("# Describe the feature you'd like to see in Oak."));
        assert!(t.contains("# Lines starting with '#' are ignored. An empty message aborts."));
        assert!(t.contains("# Name: mrmrs"));
        assert!(t.contains("# Email: adam@oak.space"));
    }

    #[test]
    fn render_template_notes_missing_email() {
        let t = render_template(None, None);
        assert!(t.contains("# Name: (not set)"));
        assert!(t.contains("# Email: (not set — pass --email or set OAK_EMAIL)"));
    }

    // --- link / unlink ------------------------------------------------------

    const ID: &str = "0123456789abcdef0123456789ab";

    fn link_fixture(owner: &str, name: &str, branch: Option<&str>) -> FeedbackLink {
        FeedbackLink {
            id: serde_json::json!("lnk-1"),
            feedback_id: serde_json::json!(ID),
            repo_owner: owner.to_string(),
            repo_name: name.to_string(),
            branch: branch.map(str::to_string),
            link_type: "branch".to_string(),
            ..Default::default()
        }
    }

    /// A link with an explicit id, branch and commit — the shapes the three
    /// unlink selectors have to tell apart.
    fn link_with(id: &str, branch: Option<&str>, commit: Option<&str>) -> FeedbackLink {
        FeedbackLink {
            id: serde_json::json!(id),
            feedback_id: serde_json::json!(ID),
            repo_owner: "mrmrs".to_string(),
            repo_name: "oak".to_string(),
            branch: branch.map(str::to_string),
            commit_hash: commit.map(str::to_string),
            link_type: if branch.is_some() { "branch" } else { "commit" }.to_string(),
            ..Default::default()
        }
    }

    fn repo() -> (String, String) {
        ("mrmrs".to_string(), "oak".to_string())
    }

    fn unlink_options(
        link_id: Option<&str>,
        commit: Option<&str>,
        branch: Option<&str>,
    ) -> FeedbackUnlinkOptions {
        FeedbackUnlinkOptions {
            item: "fb-165".to_string(),
            link_id: link_id.map(str::to_string),
            commit: commit.map(str::to_string),
            branch: branch.map(str::to_string),
            repo: None,
            remote: None,
            json: false,
        }
    }

    #[test]
    fn parse_item_ref_accepts_fb_prefix_bare_number_and_raw_id() {
        assert_eq!(parse_item_ref("fb-165").unwrap(), ItemRef::Number(165));
        assert_eq!(parse_item_ref("  FB-165 ").unwrap(), ItemRef::Number(165));
        assert_eq!(parse_item_ref("165").unwrap(), ItemRef::Number(165));
        assert_eq!(
            parse_item_ref(ID).unwrap(),
            ItemRef::Id(ID.to_string()),
            "a 28-hex id is used as-is"
        );
    }

    #[test]
    fn parse_item_ref_rejects_nonsense() {
        for raw in ["", "fb-", "fb-abc", "not an item", "fb-165-x", "abc"] {
            let err = parse_item_ref(raw).expect_err("should not parse as a feedback item");
            assert_eq!(err.exit_code(), 2, "{raw:?} is a usage error");
        }
    }

    #[test]
    fn parse_repo_slug_requires_org_and_repo() {
        assert_eq!(parse_repo_slug(" mrmrs/oak/ ").unwrap(), repo());
        for raw in ["oak", "/oak", "mrmrs/", "mrmrs/oak/extra", ""] {
            assert!(parse_repo_slug(raw).is_err(), "{raw:?} should be rejected");
        }
    }

    #[test]
    fn link_request_body_branch_link() {
        let body = link_request_body("mrmrs", "oak", Some("my-branch"), None);
        assert_eq!(body["repo_owner"], "mrmrs");
        assert_eq!(body["repo_name"], "oak");
        assert_eq!(body["branch"], "my-branch");
        assert_eq!(body["link_type"], "branch");
        assert!(body.get("commit_hash").is_none());
    }

    #[test]
    fn link_request_body_commit_only_link() {
        let body = link_request_body("mrmrs", "oak", None, Some("deadbeef"));
        assert_eq!(body["commit_hash"], "deadbeef");
        assert_eq!(body["link_type"], "commit");
        assert!(body.get("branch").is_none());
    }

    #[test]
    fn link_request_body_branch_wins_when_both_given() {
        let body = link_request_body("mrmrs", "oak", Some("my-branch"), Some("deadbeef"));
        assert_eq!(body["link_type"], "branch");
        assert_eq!(body["branch"], "my-branch");
        assert_eq!(body["commit_hash"], "deadbeef");
    }

    #[test]
    fn resolve_link_branch_prefers_flag_and_leaves_commit_only_links_alone() {
        let nowhere = Path::new("/nonexistent-oak-checkout");
        assert_eq!(
            resolve_link_branch(nowhere, Some(" my-branch "), None).unwrap(),
            Some("my-branch".to_string())
        );
        assert_eq!(
            resolve_link_branch(nowhere, None, Some("deadbeef")).unwrap(),
            None,
            "--commit alone must not pick up a default branch"
        );
        assert!(
            resolve_link_branch(nowhere, None, None).is_err(),
            "outside a checkout with neither flag there is nothing to link"
        );
    }

    // --- credential isolation ----------------------------------------------

    #[test]
    fn links_token_precedence_is_env_then_the_effective_remotes_own_credential() {
        assert_eq!(
            choose_links_token(Some("env-key"), Some("remote-key")),
            Some("env-key".to_string()),
            "OAK_API_KEY is an explicit instruction and wins"
        );
        assert_eq!(
            choose_links_token(None, Some("remote-key")),
            Some("remote-key".to_string())
        );
        assert_eq!(
            choose_links_token(None, None),
            None,
            "with no credential for this remote the command must fail closed — \
             there is deliberately no checkout-key fallback to reach for"
        );
        assert_eq!(
            choose_links_token(Some("  "), Some(" remote-key ")),
            Some("remote-key".to_string()),
            "blank values are not credentials"
        );
    }

    #[test]
    fn missing_credential_is_a_usage_error_naming_the_effective_remote() {
        let err = missing_credential("https://not-your-server.example");
        assert_eq!(err.exit_code(), 2);
        assert!(err.message().contains("https://not-your-server.example"));
        assert!(err.message().contains("oak login"));
        assert!(
            err.message().contains("never sent"),
            "the message must say a foreign credential is not reused: {}",
            err.message()
        );
    }

    // --- unlink selectors ---------------------------------------------------

    #[test]
    fn unlink_selector_order_is_link_id_then_commit_then_branch() {
        let nowhere = Path::new("/nonexistent-oak-checkout");
        assert_eq!(
            resolve_unlink_selector(nowhere, &unlink_options(Some("lnk-3"), None, None)).unwrap(),
            UnlinkSelector::LinkId("lnk-3".to_string())
        );
        assert_eq!(
            resolve_unlink_selector(nowhere, &unlink_options(None, Some(" abc123 "), None))
                .unwrap(),
            UnlinkSelector::Commit("abc123".to_string())
        );
        assert_eq!(
            resolve_unlink_selector(nowhere, &unlink_options(None, None, Some("my-branch")))
                .unwrap(),
            UnlinkSelector::Branch("my-branch".to_string())
        );
    }

    #[test]
    fn unlink_outside_a_checkout_with_no_selector_is_a_usage_error() {
        let nowhere = Path::new("/nonexistent-oak-checkout");
        let err = resolve_unlink_selector(nowhere, &unlink_options(None, None, None))
            .expect_err("nothing names a link");
        assert_eq!(err.exit_code(), 2);
        assert!(err.message().contains("--link-id"));
        assert!(err.message().contains("--commit"));
        assert!(err.message().contains("--branch"));
    }

    #[test]
    fn matching_links_by_branch_matches_repo_case_insensitively_and_branch_exactly() {
        let links = vec![
            link_fixture("mrmrs", "oak", Some("my-branch")),
            link_fixture("MRMRS", "OAK", Some("other-branch")),
            link_fixture("someone", "oak", Some("my-branch")),
            link_fixture("mrmrs", "oak", None),
        ];
        let selector = UnlinkSelector::Branch("my-branch".to_string());
        let repo = ("MrMrs".to_string(), "Oak".to_string());
        let hits = matching_links(&links, &selector, Some(&repo));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo_owner, "mrmrs");

        let cased = UnlinkSelector::Branch("My-Branch".to_string());
        assert!(
            matching_links(&links, &cased, Some(&repo)).is_empty(),
            "branch names are case-sensitive"
        );
    }

    #[test]
    fn matching_links_by_commit_reaches_commit_only_and_branch_plus_commit_links() {
        let links = vec![
            link_with("lnk-1", None, Some("ec1d378a00")),
            link_with("lnk-2", Some("my-branch"), Some("ec1d378a00")),
            link_with("lnk-3", Some("my-branch"), None),
            link_with("lnk-4", None, Some("ffffffffff")),
        ];
        let repo = repo();
        let selector = UnlinkSelector::Commit("EC1D378A00".to_string());
        let hits = matching_links(&links, &selector, Some(&repo));
        assert_eq!(
            hits.iter().filter_map(|l| l.id_str()).collect::<Vec<_>>(),
            vec!["lnk-1".to_string(), "lnk-2".to_string()],
            "a commit selector reaches both commit-only and branch+commit links, case-insensitively"
        );
    }

    #[test]
    fn matching_links_by_link_id_ignores_the_repo_filter() {
        let links = vec![
            link_with("lnk-1", Some("a"), None),
            FeedbackLink {
                repo_owner: "someone-else".to_string(),
                repo_name: "other".to_string(),
                ..link_with("lnk-2", Some("b"), None)
            },
        ];
        let selector = UnlinkSelector::LinkId("lnk-2".to_string());
        let hits = matching_links(&links, &selector, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo_name, "other");
    }

    #[test]
    fn matching_links_reports_every_duplicate_rather_than_one() {
        let links = vec![
            link_with("lnk-1", Some("my-branch"), None),
            link_with("lnk-2", Some("my-branch"), None),
        ];
        let repo = repo();
        let selector = UnlinkSelector::Branch("my-branch".to_string());
        assert_eq!(matching_links(&links, &selector, Some(&repo)).len(), 2);
    }

    #[test]
    fn choose_link_ambiguity_lists_ids_and_runnable_commands() {
        let links = vec![
            link_with("lnk-1", Some("my-branch"), None),
            link_with("lnk-2", Some("my-branch"), Some("ec1d378a00")),
        ];
        let repo = repo();
        let selector = UnlinkSelector::Branch("my-branch".to_string());
        let err = choose_link(&links, &selector, Some(&repo), "fb-165", None)
            .expect_err("two links match, so it must refuse to guess");
        assert_eq!(err.exit_code(), 1);
        let msg = err.message();
        assert!(
            msg.contains("fb-165 has 2 links to mrmrs/oak@my-branch"),
            "{msg}"
        );
        assert!(msg.contains("lnk-1"), "{msg}");
        assert!(msg.contains("lnk-2"), "{msg}");
        assert!(
            msg.contains("oak feedback unlink fb-165 --link-id lnk-1"),
            "the follow-up must be copy-pasteable: {msg}"
        );
        assert!(
            msg.contains("oak feedback unlink fb-165 --link-id lnk-2"),
            "{msg}"
        );
    }

    #[test]
    fn choose_link_no_match_names_what_the_item_is_linked_to() {
        let links = vec![link_with("lnk-1", Some("other-branch"), None)];
        let repo = repo();
        let selector = UnlinkSelector::Branch("my-branch".to_string());
        let err = choose_link(&links, &selector, Some(&repo), "fb-165", None)
            .expect_err("nothing matches");
        assert_eq!(err.exit_code(), 1);
        assert!(err.message().contains("has no link to mrmrs/oak@my-branch"));
        assert!(err.message().contains("It is linked to:"));
        assert!(err.message().contains("other-branch"));
    }

    #[test]
    fn choose_link_by_link_id_picks_exactly_that_row() {
        let links = vec![
            link_with("lnk-1", Some("my-branch"), None),
            link_with("lnk-2", Some("my-branch"), None),
        ];
        let selector = UnlinkSelector::LinkId("lnk-2".to_string());
        let chosen =
            choose_link(&links, &selector, None, "fb-165", None).expect("the id is unique");
        assert_eq!(chosen.id_str(), Some("lnk-2".to_string()));
    }

    #[test]
    fn choose_link_by_unknown_link_id_says_so() {
        let links = vec![link_with("lnk-1", Some("my-branch"), None)];
        let selector = UnlinkSelector::LinkId("lnk-9".to_string());
        let err = choose_link(&links, &selector, None, "fb-165", None).expect_err("no such link");
        assert!(err.message().contains("has no link to link lnk-9"));
        assert!(err.message().contains("lnk-1"), "it lists what does exist");
    }

    #[test]
    fn find_by_number_resolves_fb_n_to_its_id() {
        let items: Vec<FeedbackItem> = serde_json::from_str(&format!(
            r#"[{{"id":"aaaa","number":164,"ref":"fb-164","links":[]}},
                {{"id":"{ID}","number":165,"ref":"fb-165","title":"t","links":[]}}]"#
        ))
        .expect("feedback rows parse leniently");
        let found = find_by_number(&items, 165).expect("fb-165 is in the list");
        assert_eq!(found.id.as_deref(), Some(ID));
        assert_eq!(found.r#ref.as_deref(), Some("fb-165"));
        assert!(find_by_number(&items, 999).is_none());
    }

    #[test]
    fn feedback_link_parses_leniently_and_round_trips_unknown_fields() {
        let raw = format!(
            r#"{{"id":7,"feedback_id":"{ID}","repo_owner":"mrmrs","repo_name":"oak",
                 "branch":"my-branch","commit_hash":null,"link_type":"branch",
                 "created_at":"2026-07-31T00:00:00Z","created_by":"mrmrs",
                 "future_field":"kept"}}"#
        );
        let link: FeedbackLink = serde_json::from_str(&raw).expect("link JSON parses");
        assert_eq!(link.id_str(), Some("7".to_string()), "numeric ids work too");
        assert_eq!(link.link_type, "branch");
        let out = serde_json::to_value(&link).unwrap();
        assert_eq!(out["future_field"], "kept");
        assert!(
            out.get("commit_hash").is_none(),
            "absent means default in --json output"
        );
        assert!(
            out.get("source").is_none(),
            "a server that predates `source` produces no `source` key"
        );
    }

    #[test]
    fn feedback_link_surfaces_source_when_the_server_sends_it() {
        let raw = format!(
            r#"{{"id":"lnk-1","feedback_id":"{ID}","repo_owner":"mrmrs","repo_name":"oak",
                 "branch":"my-branch","link_type":"branch","source":"branch_description"}}"#
        );
        let link: FeedbackLink = serde_json::from_str(&raw).expect("link JSON parses");
        assert_eq!(link.source.as_deref(), Some("branch_description"));
        let out = serde_json::to_value(&link).unwrap();
        assert_eq!(out["source"], "branch_description");
        assert!(
            link.summary().contains("(source: branch_description)"),
            "an ambiguity listing should say where each link came from: {}",
            link.summary()
        );
    }

    #[test]
    fn feedback_link_without_an_id_cannot_be_deleted() {
        let link: FeedbackLink = serde_json::from_str(r#"{"repo_owner":"mrmrs"}"#).unwrap();
        assert_eq!(link.id_str(), None);
    }

    #[test]
    fn link_target_reads_as_org_repo_at_branch() {
        assert_eq!(
            link_target("mrmrs", "oak", Some("my-branch"), None),
            "mrmrs/oak@my-branch"
        );
        assert_eq!(
            link_target("mrmrs", "oak", None, Some("deadbeef")),
            "mrmrs/oak commit deadbeef"
        );
        assert_eq!(
            link_target("mrmrs", "oak", Some("my-branch"), Some("deadbeef")),
            "mrmrs/oak@my-branch (deadbeef)"
        );
    }

    #[test]
    fn selector_target_reads_naturally_for_each_selector() {
        let repo = repo();
        assert_eq!(
            selector_target(&UnlinkSelector::Branch("b".to_string()), Some(&repo)),
            "mrmrs/oak@b"
        );
        assert_eq!(
            selector_target(&UnlinkSelector::Commit("abc".to_string()), Some(&repo)),
            "mrmrs/oak commit abc"
        );
        assert_eq!(
            selector_target(&UnlinkSelector::LinkId("lnk-1".to_string()), None),
            "link lnk-1"
        );
    }

    #[test]
    fn unlink_hint_is_a_runnable_link_id_command() {
        let link = link_fixture("mrmrs", "oak", Some("my-branch"));
        assert_eq!(
            unlink_hint("fb-165", &link),
            "oak feedback unlink fb-165 --link-id lnk-1",
            "the undo command must work verbatim, even for a duplicated branch"
        );
    }

    #[test]
    fn unlink_hint_falls_back_when_the_server_sent_no_id() {
        let mut link = link_fixture("mrmrs", "oak", Some("my-branch"));
        link.id = serde_json::Value::Null;
        assert_eq!(
            unlink_hint("fb-165", &link),
            "oak feedback unlink fb-165 --repo mrmrs/oak --branch my-branch"
        );
        link.branch = None;
        link.commit_hash = Some("ec1d378a00".to_string());
        assert_eq!(
            unlink_hint("fb-165", &link),
            "oak feedback unlink fb-165 --repo mrmrs/oak --commit ec1d378a00"
        );
    }

    #[test]
    fn relink_hint_spells_the_link_the_server_recorded() {
        assert_eq!(
            relink_hint("fb-165", &link_with("lnk-1", Some("my-branch"), None)),
            "oak feedback link fb-165 --repo mrmrs/oak --branch my-branch"
        );
        assert_eq!(
            relink_hint("fb-165", &link_with("lnk-1", None, Some("ec1d378a00"))),
            "oak feedback link fb-165 --repo mrmrs/oak --commit ec1d378a00"
        );
        assert_eq!(
            relink_hint(
                "fb-165",
                &link_with("lnk-1", Some("my-branch"), Some("ec1d378a00"))
            ),
            "oak feedback link fb-165 --repo mrmrs/oak --branch my-branch --commit ec1d378a00"
        );
    }

    #[test]
    fn other_links_hint_lists_what_the_item_does_have() {
        assert_eq!(other_links_hint(&[]), "");
        let hint = other_links_hint(&[link_fixture("mrmrs", "oak", Some("my-branch"))]);
        assert!(hint.contains("It is linked to:"));
        assert!(hint.contains("mrmrs/oak"));
        assert!(hint.contains("my-branch"));
    }

    #[cfg(unix)]
    fn shell_argv(command: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("oak() {{ printf '%s\\0' \"$@\"; }}\n{command}"))
            .current_dir(dir.path())
            .env_remove("ENV")
            .env_remove("BASH_ENV")
            .output()
            .unwrap();
        assert!(
            !dir.path().join("qa_injected").exists(),
            "command executed injected shell code"
        );
        assert!(
            out.status.success(),
            "shell command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn generated_commands_preserve_every_argument_through_a_real_shell() {
        for value in [
            "x; touch qa_injected;",
            "x'quote\"",
            "$(touch qa_injected)",
            "`touch qa_injected`",
            "two words",
            "line\nbreak",
        ] {
            let mut link = link_with(value, Some(value), Some(value));
            link.repo_owner = value.to_string();
            link.repo_name = value.to_string();
            let repo = format!("{value}/{value}");
            let remote = "https://fixture.invalid/base;$(touch qa_injected)";
            let message = ambiguous_message(value, "target", &[&link, &link], Some(remote));
            let commands = message
                .split_once("Re-run naming the one you mean:\n")
                .unwrap()
                .1;
            let expected = [
                "feedback",
                "unlink",
                value,
                "--link-id",
                value,
                "--remote",
                remote,
            ];
            assert_eq!(
                shell_argv(commands),
                expected
                    .iter()
                    .chain(expected.iter())
                    .copied()
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                shell_argv(&at_remote(relink_hint(value, &link), remote)),
                [
                    "feedback", "link", value, "--repo", &repo, "--branch", value, "--commit",
                    value, "--remote", remote
                ]
            );
            assert_eq!(
                shell_argv(&at_remote(unlink_hint(value, &link), remote)),
                [
                    "feedback",
                    "unlink",
                    value,
                    "--link-id",
                    value,
                    "--remote",
                    remote
                ]
            );
            link.id = serde_json::Value::Null;
            assert_eq!(
                shell_argv(&at_remote(unlink_hint(value, &link), remote)),
                [
                    "feedback", "unlink", value, "--repo", &repo, "--branch", value, "--remote",
                    remote
                ]
            );
            link.branch = None;
            assert_eq!(
                shell_argv(&at_remote(unlink_hint(value, &link), remote)),
                [
                    "feedback", "unlink", value, "--repo", &repo, "--commit", value, "--remote",
                    remote
                ]
            );
        }
    }
}
