//! `oak mark`: named pointers to a commit on `main` that people and bots move
//! to record what has passed what (`server-good`, `cli-approved`), and that
//! target machines follow to deploy (`oak mark follow`).
//!
//! Marks live on the server (`/api/:owner/:name/marks`). A mark moves
//! forward along `main`'s first-parent history, and only when the marks it
//! requires are already at or past the commit; moving it back is a rollback,
//! which takes a repo admin (or its owners, when the mark allows it). Every
//! move is signed with the server's release grant key, and `oak mark follow`
//! hands the grant to its script (`OAK_GRANT`, `OAK_GRANT_SIGNATURE`) along
//! with `OAK_COMMIT`, `OAK_MOVE_ID` and the rest.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::access::RepoTarget;
use super::branch_target::bounded_body;
use super::report::{post_report, ReportArgs};
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The most of a script's output a report carries, from its end.
const LOG_TAIL_BYTES: usize = 64 * 1024;

/// The variables a follower's script may get from the server, besides the
/// ones `oak` sets itself. Anything else a server sends is ignored.
/// `OAK_GRANT` is the move's signed grant (base64), `OAK_GRANT_SIGNATURE`
/// its minisign signature (base64) and `OAK_REPO_ID` the repo's numeric id.
pub const FOLLOW_ENV_NAMES: [&str; 3] = ["OAK_REPO_ID", "OAK_GRANT", "OAK_GRANT_SIGNATURE"];

/// Deprecated: the names the deploy scripts read when marks replaced Oak
/// CI. Still passed through (and filled in from the new names) because a
/// deploy machine may run a script installed outside the repo, or roll back
/// to a commit whose scripts read them. Remove once nothing reads them.
pub const DEPRECATED_FOLLOW_ENV_NAMES: [&str; 13] = [
    "OAK_CI_REPO_ID",
    "OAK_CI_COMMIT",
    "OAK_CI_BRANCH",
    "OAK_CI_WORKFLOW_PATH",
    "OAK_CI_WORKFLOW_HASH",
    "OAK_CI_EVENT",
    "OAK_CI_RUN_ID",
    "OAK_CI_AUTHORIZATION_ID",
    "OAK_CI_ENVIRONMENT_ID",
    "OAK_CI_ENVIRONMENT_NAME",
    "OAK_CI_POLICY_VERSION",
    "OAK_CI_RELEASE_GRANT",
    "OAK_CI_RELEASE_GRANT_SIGNATURE",
];

/// Each new name and the deprecated one carrying the same value.
const RENAMED_FOLLOW_ENV: [(&str, &str); 3] = [
    ("OAK_REPO_ID", "OAK_CI_REPO_ID"),
    ("OAK_GRANT", "OAK_CI_RELEASE_GRANT"),
    ("OAK_GRANT_SIGNATURE", "OAK_CI_RELEASE_GRANT_SIGNATURE"),
];

/// Names `oak mark follow` sets itself.
const OWN_FOLLOW_ENV_NAMES: [&str; 7] = [
    "OAK_COMMIT",
    "OAK_MARK",
    "OAK_MOVE_ID",
    "OAK_MOVE_KIND",
    "OAK_LINE",
    "OAK_REPO",
    "OAK_REMOTE",
];

/// One mark, as the server describes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkWire {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_move_id: Option<i64>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub required_approvers: Vec<String>,
    #[serde(default)]
    pub allow_group_rollback: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followers: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub commit_hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_identity: Option<Value>,
    #[serde(default)]
    pub can_move: bool,
    #[serde(default)]
    pub can_roll_back: bool,
    #[serde(default)]
    pub can_edit: bool,
}

/// One move.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveWire {
    pub id: i64,
    pub mark: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    pub mover: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default)]
    pub authorization_id: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<Value>,
}

fn error_for(target: &RepoTarget, status: reqwest::StatusCode, body: &[u8]) -> OakError {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return OakError::AuthRequired {
            remote: target.remote.clone(),
            status: status.as_u16(),
        };
    }
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| {
            if status == reqwest::StatusCode::NOT_FOUND {
                format!("not found on {} (HTTP 404)", target.slug())
            } else {
                format!("HTTP {status}")
            }
        });
    OakError::Server(message)
}

pub(crate) async fn send(target: &RepoTarget, req: reqwest::RequestBuilder) -> Result<Value> {
    let response = req
        .send()
        .await
        .map_err(|e| OakError::Http(format!("marks on {}: {e}", target.slug())))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        return Err(error_for(target, status, &body));
    }
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&body)
        .map_err(|e| OakError::Server(format!("unreadable marks response: {e}")))
}

fn mark_path(mark: &str, tail: &str) -> String {
    let base = format!("marks/{}", urlencoding::encode(mark));
    if tail.is_empty() {
        base
    } else {
        format!("{base}/{tail}")
    }
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn behind_text(m: &MarkWire) -> String {
    match m.behind {
        Some(0) => "at head".to_string(),
        Some(n) => format!("{n} behind"),
        None => "far behind".to_string(),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value, what: &str) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|e| OakError::Server(format!("unreadable {what} from the server: {e}")))
}

/// `oak mark list`.
pub async fn list(target: RepoTarget, json: bool) -> Result<()> {
    let value = send(
        &target,
        target.request(reqwest::Method::GET, target.url("marks"), REQUEST_TIMEOUT),
    )
    .await?;
    let marks: Vec<MarkWire> = parse(value["marks"].clone(), "marks")?;
    let slug = target.slug();
    if json {
        return output::print_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "repo": slug,
            "marks": marks,
            "recommended_next_commands": marks.first().map(|m| vec![format!(
                "oak mark log {} --repo {} --json",
                output::shell_quote(&m.name),
                output::shell_quote(&slug)
            )]).unwrap_or_default(),
        }));
    }
    if marks.is_empty() {
        output::print_line(&format!("{slug} has no marks yet."));
        return Ok(());
    }
    for m in &marks {
        let owner = match (&m.owner_group, m.owner.as_str()) {
            (Some(g), _) => format!("@group/{g}"),
            (None, "group") => "admins only".to_string(),
            _ => "writers".to_string(),
        };
        let line = m
            .root
            .as_deref()
            .map(|r| format!(" on ~{r}"))
            .unwrap_or_default();
        let requires = if m.requires.is_empty() {
            String::new()
        } else {
            format!("  requires {}", m.requires.join(", "))
        };
        output::print_line(&format!(
            "{:<24} {}  {}{line}  {owner}{requires}",
            m.name,
            short(&m.commit),
            behind_text(m)
        ));
    }
    Ok(())
}

/// `oak mark show MARK`.
pub async fn show(target: RepoTarget, mark: &str, json: bool) -> Result<()> {
    let value = send(
        &target,
        target.request(
            reqwest::Method::GET,
            target.url(&mark_path(mark, "")),
            REQUEST_TIMEOUT,
        ),
    )
    .await?;
    if json {
        let mut doc = value.clone();
        doc["schema_version"] = json!(SCHEMA_VERSION);
        doc["repo"] = json!(target.slug());
        return output::print_json(&doc);
    }
    let m: MarkWire = parse(value["mark"].clone(), "mark")?;
    output::print_line(&format!("{}  {}  {}", m.name, m.commit, behind_text(&m)));
    if !m.description.is_empty() {
        output::print_line(&m.description);
    }
    output::print_line(&format!(
        "Moved by: {}",
        match &m.owner_group {
            Some(g) => format!("@group/{g} (and repo admins)"),
            None if m.owner == "group" => "repo admins (its group was deleted)".to_string(),
            None => "anyone who can write the repo".to_string(),
        }
    ));
    output::print_line(&format!(
        "Rolled back by: {}",
        if m.allow_group_rollback {
            "its owners and repo admins"
        } else {
            "repo admins"
        }
    ));
    if !m.requires.is_empty() {
        output::print_line(&format!("Requires: {}", m.requires.join(", ")));
    }
    if !m.required_approvers.is_empty() {
        output::print_line(&format!("Approvers: {}", m.required_approvers.join(", ")));
    }
    if !m.followers.is_empty() {
        output::print_line(&format!("Followers: {}", m.followers.join(", ")));
    }
    if m.legacy_identity.is_some() {
        output::print_line(
            "Grants: carry a fixed identity set through the API (an existing deploy box checks it)",
        );
    }
    if let Ok(mv) = serde_json::from_value::<MoveWire>(value["current_move"].clone()) {
        output::print_line(&format!(
            "Last move: #{} {} by {} at {}{}",
            mv.id,
            mv.kind,
            mv.mover,
            mv.created_at,
            if mv.note.is_empty() {
                String::new()
            } else {
                format!(" ({})", mv.note)
            }
        ));
    }
    Ok(())
}

/// What `oak mark create` sends.
#[derive(Debug, Default)]
pub struct CreateArgs {
    pub name: String,
    pub commit: Option<String>,
    pub owner_group: Option<String>,
    pub root: Option<String>,
    pub requires: Vec<String>,
    pub approvers: Vec<String>,
    pub allow_group_rollback: bool,
    pub description: Option<String>,
    pub followers: Vec<String>,
}

/// `oak mark create NAME`.
pub async fn create(target: RepoTarget, args: CreateArgs, json: bool) -> Result<()> {
    let mut body = json!({ "name": args.name });
    if let Some(c) = &args.commit {
        body["commit"] = json!(c);
    }
    if let Some(g) = &args.owner_group {
        body["owner_group"] = json!(g);
    }
    if let Some(r) = &args.root {
        body["root"] = json!(r);
    }
    if !args.requires.is_empty() {
        body["requires"] = json!(args.requires);
    }
    if !args.approvers.is_empty() {
        body["required_approvers"] = json!(args.approvers);
    }
    if args.allow_group_rollback {
        body["allow_group_rollback"] = json!(true);
    }
    if let Some(d) = &args.description {
        body["description"] = json!(d);
    }
    if !args.followers.is_empty() {
        body["followers"] = json!(args.followers);
    }
    let value = send(
        &target,
        target
            .request(reqwest::Method::POST, target.url("marks"), REQUEST_TIMEOUT)
            .json(&body),
    )
    .await?;
    report_move(&target, value, "create", json)
}

fn report_move(target: &RepoTarget, value: Value, action: &str, json: bool) -> Result<()> {
    let m: MarkWire = parse(value["mark"].clone(), "mark")?;
    let mv: MoveWire = parse(value["move"].clone(), "move")?;
    if json {
        return output::print_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "repo": target.slug(),
            "action": action,
            "mark": m,
            "move": mv,
            "recommended_next_commands": [format!(
                "oak mark log {} --repo {} --json",
                output::shell_quote(&m.name),
                output::shell_quote(&target.slug())
            )],
        }));
    }
    let verb = match mv.kind.as_str() {
        "create" => "Created",
        "rollback" => "Rolled back",
        _ => "Moved",
    };
    let prep = if mv.kind == "create" { "at" } else { "to" };
    output::success(&format!(
        "{verb} {} {prep} {} (move #{}, {})",
        m.name,
        short(&mv.to),
        mv.id,
        behind_text(&m)
    ));
    Ok(())
}

/// `oak mark move MARK COMMIT` / `oak mark rollback MARK COMMIT`.
pub async fn move_to(
    target: RepoTarget,
    mark: &str,
    commit: &str,
    note: Option<&str>,
    rollback: bool,
    json: bool,
) -> Result<()> {
    let tail = if rollback { "rollback" } else { "move" };
    let mut body = json!({ "commit": commit });
    if let Some(n) = note {
        body["note"] = json!(n);
    }
    let value = send(
        &target,
        target
            .request(
                reqwest::Method::POST,
                target.url(&mark_path(mark, tail)),
                REQUEST_TIMEOUT,
            )
            .json(&body),
    )
    .await?;
    report_move(&target, value, tail, json)
}

/// `oak mark delete MARK`.
pub async fn delete(target: RepoTarget, mark: &str, json: bool) -> Result<()> {
    send(
        &target,
        target.request(
            reqwest::Method::DELETE,
            target.url(&mark_path(mark, "")),
            REQUEST_TIMEOUT,
        ),
    )
    .await?;
    if json {
        return output::print_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "repo": target.slug(),
            "action": "delete",
            "mark": mark,
            "recommended_next_commands": [format!("oak mark list --repo {} --json", output::shell_quote(&target.slug()))],
        }));
    }
    output::success(&format!("Deleted {mark} and its moves"));
    Ok(())
}

/// `oak mark log MARK`.
pub async fn log(target: RepoTarget, mark: &str, limit: u32, json: bool) -> Result<()> {
    let url = format!(
        "{}?limit={}",
        target.url(&mark_path(mark, "moves")),
        limit.clamp(1, 200)
    );
    let value = send(
        &target,
        target.request(reqwest::Method::GET, url, REQUEST_TIMEOUT),
    )
    .await?;
    let moves: Vec<MoveWire> = parse(value["moves"].clone(), "moves")?;
    if json {
        return output::print_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "repo": target.slug(),
            "mark": mark,
            "moves": moves,
            "recommended_next_commands": Vec::<String>::new(),
        }));
    }
    for mv in &moves {
        let from = mv
            .from
            .as_deref()
            .map(|f| format!("{} -> ", short(f)))
            .unwrap_or_default();
        let reports: Vec<String> = mv
            .reports
            .iter()
            .filter_map(|r| {
                let name = r.get("name")?.as_str()?;
                // Older servers sent a state instead of a headline.
                let said = r
                    .get("headline")
                    .or_else(|| r.get("state"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                Some(if said.is_empty() {
                    name.to_string()
                } else {
                    format!("{name}: {said}")
                })
            })
            .collect();
        output::print_line(&format!(
            "#{:<6} {:<8} {from}{}  {}  {}{}{}",
            mv.id,
            mv.kind,
            short(&mv.to),
            mv.mover,
            mv.created_at,
            if mv.note.is_empty() {
                String::new()
            } else {
                format!("  \"{}\"", mv.note)
            },
            if reports.is_empty() {
                String::new()
            } else {
                format!("  [{}]", reports.join(", "))
            }
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Following
// ---------------------------------------------------------------------------

/// What `oak mark follow` was asked to do.
#[derive(Debug, Clone)]
pub struct FollowArgs {
    pub mark: String,
    /// The script, relative to the work dir or absolute, then its arguments.
    pub run: Vec<String>,
    pub dir: Option<PathBuf>,
    pub state_file: Option<PathBuf>,
    pub once: bool,
    pub checkout: bool,
    /// Run the script even when the server sends no grant or move
    /// variables (this credential isn't one of the mark's followers).
    pub no_grant: bool,
    pub report_name: Option<String>,
    /// Seconds each request waits (tests shorten it).
    pub wait_seconds: u64,
}

/// The follower's memory: the last move it handled, and the one it is
/// running (so a restart can say that one was interrupted).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FollowState {
    pub mark: String,
    #[serde(default)]
    pub last_move_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_move_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_commit: Option<String>,
}

impl FollowState {
    pub fn load(path: &Path, mark: &str) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let state: FollowState = serde_json::from_slice(&bytes).map_err(|e| {
                    OakError::InvalidArgument(format!(
                        "{} isn't a follower state file ({e}); move it away to start over",
                        path.display()
                    ))
                })?;
                if state.mark != mark {
                    return Err(OakError::InvalidArgument(format!(
                        "{} belongs to mark {}, not {mark}",
                        path.display(),
                        state.mark
                    )));
                }
                Ok(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FollowState {
                mark: mark.to_string(),
                ..FollowState::default()
            }),
            Err(e) => Err(OakError::Io(e)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// An exclusive lock on the state file, held while following, so two
/// followers never run moves of the same mark at once.
struct FollowLock {
    #[allow(dead_code)]
    file: std::fs::File,
}

impl FollowLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: flock on a descriptor this struct owns.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                return Err(OakError::InvalidArgument(format!(
                    "another `oak mark follow` holds {}; one follower per state file",
                    path.display()
                )));
            }
        }
        Ok(FollowLock { file })
    }
}

/// The follower's answer from the server.
#[derive(Debug, Clone, Deserialize)]
pub struct FollowWire {
    pub mark: String,
    #[serde(rename = "move", default)]
    pub mv: Option<MoveWire>,
    #[serde(default)]
    pub timed_out: bool,
    /// The move is too old for its grant to be signed again.
    #[serde(default)]
    pub grant_expired: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub line: Option<String>,
}

/// The environment a follower's script runs with: `OAK_COMMIT`,
/// `OAK_MARK`, `OAK_MOVE_ID`, `OAK_MOVE_KIND`, `OAK_LINE`, `OAK_REPO`,
/// `OAK_REMOTE`, and the allow-listed variables the server sent for the
/// move (`OAK_REPO_ID`, `OAK_GRANT`, `OAK_GRANT_SIGNATURE`, and the
/// deprecated `OAK_CI_*` aliases).
pub fn follow_env(
    target: &RepoTarget,
    wire: &FollowWire,
    mv: &MoveWire,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = wire
        .env
        .iter()
        .filter(|(k, v)| {
            (FOLLOW_ENV_NAMES.contains(&k.as_str())
                || DEPRECATED_FOLLOW_ENV_NAMES.contains(&k.as_str()))
                && !v.contains('\0')
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // A server from before the rename sends only the deprecated names, and
    // a later one may send only the new ones: fill in whichever is missing.
    for (new, old) in RENAMED_FOLLOW_ENV {
        match (env.get(new).cloned(), env.get(old).cloned()) {
            (None, Some(v)) => {
                env.insert(new.into(), v);
            }
            (Some(v), None) => {
                env.insert(old.into(), v);
            }
            _ => {}
        }
    }
    env.insert("OAK_COMMIT".into(), mv.to.clone());
    env.insert("OAK_MARK".into(), wire.mark.clone());
    env.insert("OAK_MOVE_ID".into(), mv.id.to_string());
    env.insert("OAK_MOVE_KIND".into(), mv.kind.clone());
    env.insert(
        "OAK_LINE".into(),
        wire.line.clone().unwrap_or_else(|| "main".to_string()),
    );
    env.insert("OAK_REPO".into(), target.slug());
    env.insert("OAK_REMOTE".into(), target.remote.clone());
    env
}

/// Keeps the end of a script's output while it streams through.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > LOG_TAIL_BYTES * 2 {
            let cut = self.bytes.len() - LOG_TAIL_BYTES;
            self.bytes.drain(..cut);
        }
    }

    fn text(&self) -> String {
        let start = self.bytes.len().saturating_sub(LOG_TAIL_BYTES);
        String::from_utf8_lossy(&self.bytes[start..]).into_owned()
    }
}

fn pump<R: Read + Send + 'static>(
    mut from: R,
    to_stderr: bool,
    tail: Arc<Mutex<Tail>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_stderr {
                        let _ = std::io::stderr().write_all(&buf[..n]);
                    } else {
                        let _ = std::io::stdout().write_all(&buf[..n]);
                    }
                    if let Ok(mut t) = tail.lock() {
                        t.push(&buf[..n]);
                    }
                }
            }
        }
    })
}

/// What a follower says when a move's script ends: `live` or `failed` with
/// the move, then the end of the script's output.
fn follow_report_body(state: &str, move_id: i64, output: &str) -> String {
    let output = output.trim_end();
    if output.is_empty() {
        format!("{state} (move #{move_id})")
    } else {
        format!("{state} (move #{move_id})\n\n{output}")
    }
}

/// Run `run` in `dir` with `env`, streaming its output; `(success, tail)`.
/// With `quiet_stdout` (under `--json`) the script's stdout goes to stderr,
/// so stdout carries only the receipt.
fn run_script(
    dir: &Path,
    run: &[String],
    env: &BTreeMap<String, String>,
    quiet_stdout: bool,
) -> Result<(bool, String)> {
    let (program, args) = run
        .split_first()
        .ok_or_else(|| OakError::InvalidArgument("--run needs a script".to_string()))?;
    let program_path = Path::new(program);
    let resolved = if program_path.is_absolute() || !program.contains('/') {
        program_path.to_path_buf()
    } else {
        dir.join(program_path)
    };
    let mut cmd = Command::new(&resolved);
    cmd.args(args).current_dir(dir);
    // Never let a stale value from the follower's own environment through.
    for name in FOLLOW_ENV_NAMES
        .iter()
        .chain(DEPRECATED_FOLLOW_ENV_NAMES.iter())
        .chain(OWN_FOLLOW_ENV_NAMES.iter())
    {
        cmd.env_remove(name);
    }
    cmd.envs(env);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        OakError::InvalidArgument(format!("couldn't run {}: {e}", resolved.display()))
    })?;
    let tail = Arc::new(Mutex::new(Tail::default()));
    let out = child
        .stdout
        .take()
        .map(|s| pump(s, quiet_stdout, tail.clone()));
    let err = child.stderr.take().map(|s| pump(s, true, tail.clone()));
    let status = child.wait()?;
    for t in [out, err].into_iter().flatten() {
        let _ = t.join();
    }
    let mut text = tail.lock().map(|t| t.text()).unwrap_or_default();
    if !status.success() {
        text.push_str(&format!(
            "\n[oak mark follow] script exited with {status}\n"
        ));
    }
    Ok((status.success(), text))
}

/// Run the `oak` binary itself (clone, fetch, restore) for the checkout.
fn oak(dir: &Path, args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let out = Command::new(exe)
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(OakError::Server(format!(
            "`oak {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Put `commit` of `main` in `dir`: clone the repo there the first time,
/// then fetch `main` and restore the whole tree to the commit.
fn checkout(slug: &str, remote: &str, dir: &Path, commit: &str) -> Result<()> {
    if !dir.join(".oak").exists() {
        let parent = dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let dest = dir.to_string_lossy().into_owned();
        oak(parent, &["clone", slug, &dest, "-r", remote])?;
    } else {
        oak(dir, &["fetch"])?;
    }
    oak(dir, &["restore", "--source", commit, "--force"])
}

fn default_dir(mark: &str) -> PathBuf {
    PathBuf::from(format!("oak-mark-{mark}"))
}

/// `oak mark follow MARK --run SCRIPT`.
pub async fn follow(target: RepoTarget, args: FollowArgs, json: bool) -> Result<()> {
    let mark = args.mark.trim().to_string();
    let dir = args.dir.clone().unwrap_or_else(|| default_dir(&mark));
    let dir = if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir()?.join(dir)
    };
    let state_path = args.state_file.clone().unwrap_or_else(|| {
        let mut p = dir.clone().into_os_string();
        p.push(".follow.json");
        PathBuf::from(p)
    });
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock_path = state_path.clone().into_os_string();
    lock_path.push(".lock");
    let _lock = FollowLock::acquire(Path::new(&lock_path))?;
    let report_name = args.report_name.clone().unwrap_or_else(|| mark.clone());
    let mut state = FollowState::load(&state_path, &mark)?;

    // A move that was running when the follower stopped is not run again:
    // its authorization may already be spent. Say so and wait for the next.
    if let Some(interrupted) = state.running_move_id.take() {
        if let Some(commit) = state.running_commit.take() {
            let _ = post_report(
                &target,
                &ReportArgs {
                    commit,
                    name: report_name.clone(),
                    body: format!(
                        "failed (move #{interrupted})\n\nThe follower stopped while running it; move the mark again to retry."
                    ),
                    move_id: Some(interrupted),
                    ..ReportArgs::default()
                },
            )
            .await;
        }
        state.last_move_id = state.last_move_id.max(interrupted);
        state.save(&state_path)?;
        output::warning(&format!(
            "Move #{interrupted} of {mark} was interrupted; it won't run again"
        ));
    }

    let mut backoff = Duration::from_secs(2);
    loop {
        let url = format!(
            "{}?after={}&timeout={}",
            target.url(&mark_path(&mark, "follow")),
            state.last_move_id,
            args.wait_seconds
        );
        let answer = send(
            &target,
            target.request(
                reqwest::Method::GET,
                url,
                Duration::from_secs(args.wait_seconds + 30),
            ),
        )
        .await;
        let wire: FollowWire = match answer.and_then(|v| parse(v, "follow answer")) {
            Ok(w) => {
                backoff = Duration::from_secs(2);
                w
            }
            Err(e) if args.once => return Err(e),
            Err(e) => {
                output::warning(&format!("following {mark}: {e}; retrying"));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        let Some(mv) = wire.mv.clone() else {
            if args.once {
                if json {
                    output::print_json(&json!({
                        "schema_version": SCHEMA_VERSION,
                        "repo": target.slug(),
                        "mark": mark,
                        "handled": Value::Null,
                        "last_move_id": state.last_move_id,
                        "recommended_next_commands": Vec::<String>::new(),
                    }))?;
                } else {
                    output::info(&format!("{mark} didn't move"));
                }
                return Ok(());
            }
            continue;
        };
        if mv.id <= state.last_move_id {
            continue;
        }
        let ok = handle_move(
            &target,
            &args,
            &dir,
            &state_path,
            &mut state,
            &report_name,
            &wire,
            &mv,
            json,
        )
        .await?;
        if args.once {
            if !ok {
                return Err(OakError::Server(format!(
                    "the script failed for move #{} of {mark}",
                    mv.id
                )));
            }
            if json {
                output::print_json(&json!({
                    "schema_version": SCHEMA_VERSION,
                    "repo": target.slug(),
                    "mark": mark,
                    "handled": mv,
                    "succeeded": ok,
                    "last_move_id": state.last_move_id,
                    "recommended_next_commands": Vec::<String>::new(),
                }))?;
            }
            return Ok(());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_move(
    target: &RepoTarget,
    args: &FollowArgs,
    dir: &Path,
    state_path: &Path,
    state: &mut FollowState,
    report_name: &str,
    wire: &FollowWire,
    mv: &MoveWire,
    json: bool,
) -> Result<bool> {
    output::info(&format!(
        "{} moved to {} (move #{}, {} by {})",
        wire.mark,
        short(&mv.to),
        mv.id,
        mv.kind,
        mv.mover
    ));
    if wire.env.is_empty() && !wire.grant_expired {
        if !args.no_grant {
            return Err(OakError::InvalidArgument(format!(
                "the server sent move #{} of {} without a grant or move variables: this \
                 credential isn't one of the mark's followers (or can't see that commit). Use a \
                 follower's key, or pass --no-grant if the script doesn't need them",
                mv.id, wire.mark
            )));
        }
        output::warning(&format!(
            "--no-grant: the script for {} gets no grant or OAK_REPO_ID",
            wire.mark
        ));
    }
    state.running_move_id = Some(mv.id);
    state.running_commit = Some(mv.to.clone());
    state.save(state_path)?;
    let report = |body: String| ReportArgs {
        commit: mv.to.clone(),
        name: report_name.to_string(),
        body,
        url: None,
        move_id: Some(mv.id),
    };
    if let Err(e) = post_report(
        target,
        &report(format!("deploying (move #{} by {})", mv.id, mv.mover)),
    )
    .await
    {
        output::warning(&format!("couldn't report deploying: {e}"));
    }
    let on_main = wire.line.as_deref().is_none_or(|l| l == "main");
    let prepared = if wire.grant_expired {
        Err(OakError::Server(format!(
            "move #{} is too old for its grant to be signed again; move {} again to deploy it",
            mv.id, wire.mark
        )))
    } else if args.checkout && !on_main {
        Err(OakError::InvalidArgument(format!(
            "{} points into {}; follow checks out main commits only (use --no-checkout)",
            wire.mark,
            wire.line.as_deref().unwrap_or("a root")
        )))
    } else if args.checkout {
        let (slug, remote, dir, commit) = (
            target.slug(),
            target.remote.clone(),
            dir.to_path_buf(),
            mv.to.clone(),
        );
        tokio::task::spawn_blocking(move || checkout(&slug, &remote, &dir, &commit))
            .await
            .map_err(|e| OakError::Server(format!("checkout task: {e}")))
            .and_then(|r| r)
    } else {
        std::fs::create_dir_all(dir).map_err(OakError::Io)
    };
    let (ok, tail) = match prepared {
        Ok(()) => {
            let env = follow_env(target, wire, mv);
            let run = args.run.clone();
            let dir = dir.to_path_buf();
            match tokio::task::spawn_blocking(move || run_script(&dir, &run, &env, json)).await {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => (false, format!("[oak mark follow] {e}\n")),
                Err(e) => (false, format!("[oak mark follow] {e}\n")),
            }
        }
        Err(e) => (false, format!("[oak mark follow] checkout failed: {e}\n")),
    };
    let st = if ok { "live" } else { "failed" };
    if let Err(e) = post_report(target, &report(follow_report_body(st, mv.id, &tail))).await {
        output::warning(&format!("couldn't report {st}: {e}"));
    }
    state.running_move_id = None;
    state.running_commit = None;
    state.last_move_id = mv.id;
    state.save(state_path)?;
    if ok {
        output::success(&format!(
            "{} #{} is live at {}",
            wire.mark,
            mv.id,
            short(&mv.to)
        ));
    } else {
        output::warning(&format!(
            "{} #{} failed at {}",
            wire.mark,
            mv.id,
            short(&mv.to)
        ));
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> RepoTarget {
        RepoTarget::for_spec("https://oak.example", "acme/demo").unwrap()
    }

    #[test]
    fn follow_env_takes_only_known_names() {
        let wire: FollowWire = serde_json::from_value(json!({
            "mark": "server-prod",
            "move": {"id": 7, "mark": "server-prod", "kind": "forward", "to": "c".repeat(64),
                     "mover": "bot", "created_at": "2026-11-16T00:00:00Z"},
            "env": {"OAK_CI_RUN_ID": "7", "OAK_GRANT": "Zw==", "OAK_REPO_ID": "42",
                    "LD_PRELOAD": "/evil.so", "PATH": "/evil"},
            "line": "main",
        }))
        .unwrap();
        let mv = wire.mv.clone().unwrap();
        let env = follow_env(&target(), &wire, &mv);
        assert_eq!(env["OAK_CI_RUN_ID"], "7");
        assert_eq!(env["OAK_MOVE_ID"], "7");
        assert_eq!(env["OAK_MARK"], "server-prod");
        assert_eq!(env["OAK_REPO"], "acme/demo");
        assert_eq!(env["OAK_LINE"], "main");
        assert_eq!(env["OAK_GRANT"], "Zw==");
        // The deprecated aliases are filled in from the new names.
        assert_eq!(env["OAK_CI_RELEASE_GRANT"], "Zw==");
        assert_eq!(env["OAK_CI_REPO_ID"], "42");
        assert!(!env.contains_key("LD_PRELOAD"));
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn follow_env_fills_new_names_from_an_older_server() {
        let wire: FollowWire = serde_json::from_value(json!({
            "mark": "server-prod",
            "move": {"id": 9, "mark": "server-prod", "kind": "forward", "to": "d".repeat(64),
                     "mover": "bot", "created_at": "2026-11-16T00:00:00Z"},
            "env": {"OAK_CI_REPO_ID": "42", "OAK_CI_RELEASE_GRANT": "Zw==",
                    "OAK_CI_RELEASE_GRANT_SIGNATURE": "cw=="},
        }))
        .unwrap();
        let mv = wire.mv.clone().unwrap();
        let env = follow_env(&target(), &wire, &mv);
        assert_eq!(env["OAK_REPO_ID"], "42");
        assert_eq!(env["OAK_GRANT"], "Zw==");
        assert_eq!(env["OAK_GRANT_SIGNATURE"], "cw==");
        assert_eq!(env["OAK_CI_RELEASE_GRANT"], "Zw==");
    }

    #[test]
    fn state_files_round_trip_and_belong_to_one_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let fresh = FollowState::load(&path, "m").unwrap();
        assert_eq!(fresh.last_move_id, 0);
        let s = FollowState {
            mark: "m".into(),
            last_move_id: 4,
            running_move_id: Some(5),
            running_commit: Some("c".into()),
        };
        s.save(&path).unwrap();
        assert_eq!(FollowState::load(&path, "m").unwrap(), s);
        assert!(FollowState::load(&path, "other").is_err());
    }

    #[test]
    fn tails_keep_the_end() {
        let mut t = Tail::default();
        for _ in 0..3000 {
            t.push(&[b'x'; 100]);
        }
        t.push(b"END");
        let text = t.text();
        assert!(text.len() <= LOG_TAIL_BYTES);
        assert!(text.ends_with("END"));
    }
}
