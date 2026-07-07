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

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

use super::credentials::{get_token_for_server, preferred_author_name};
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
    if let Some(token) = get_token_for_server(&remote) {
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
        Err(e) => {
            output::error(&format!("could not parse server response: {e}"));
            std::process::exit(1);
        }
    };

    if opts.json {
        println!(
            "{}",
            serde_json::json!({ "id": parsed.id, "ref": parsed.r#ref, "status": parsed.status })
        );
    } else {
        match (&parsed.r#ref, &parsed.id) {
            (Some(r), _) => println!("↟ feedback sent — filed as {r}. Thank you!"),
            (None, Some(id)) => println!("↟ feedback sent — thank you! (id: {id})"),
            (None, None) => println!("↟ feedback sent — thank you!"),
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
}
