//! `oak mcp`: an MCP (Model Context Protocol) server over stdio, so an agent's
//! MCP client can use Oak without a shell.
//!
//! Messages are newline-delimited JSON-RPC 2.0 on stdin/stdout. Each tool runs
//! this same binary with `--json`, in the server's working directory, so a tool
//! and the command it wraps can't drift apart and nothing a command prints can
//! reach the protocol stream.
//!
//! Tools are read-only unless the server was started with `--allow-write`, and
//! even then only commit, describe and push. Merging and closing change what
//! the team sees and aren't exposed: those stay a deliberate CLI step.

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oak_core::Result;
use serde_json::{json, Value};

/// Newest first. A client asking for one of these gets it; any other gets the
/// newest, as the protocol says. They don't differ in what this server uses.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// The most of a tool's stdout or stderr read, so a shaped result can still
/// parse whole.
const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
/// The most text handed to the model from one tool call.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const DEFAULT_DIFF_BYTES: u64 = 200_000;
const DEFAULT_LOG_BYTES: u64 = 64 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "Oak is a version control system. Each branch is one task, and its \
description is the narrative of the work: commits carry no message. Start with oak_agent_state, \
or oak_branch_triage to sort a fleet of branches.";

struct Param {
    name: &'static str,
    kind: &'static str,
    description: &'static str,
    required: bool,
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> Param {
    Param {
        name,
        kind,
        description,
        required: false,
    }
}

const fn required(name: &'static str, kind: &'static str, description: &'static str) -> Param {
    Param {
        name,
        kind,
        description,
        required: true,
    }
}

/// How to run a tool: arguments for `oak`, and optionally text for its stdin.
#[derive(Debug, PartialEq)]
struct Invocation {
    args: Vec<String>,
    stdin: Option<String>,
}

impl Invocation {
    fn new(args: &[&str]) -> Self {
        Self {
            args: args.iter().map(|arg| arg.to_string()).collect(),
            stdin: None,
        }
    }

    fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add `flag value` when the value is there.
    fn option(self, flag: &str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.arg(flag).arg(value),
            None => self,
        }
    }

    fn flag(self, flag: &str, on: bool) -> Self {
        if on {
            self.arg(flag)
        } else {
            self
        }
    }
}

struct Tool {
    name: &'static str,
    description: &'static str,
    params: &'static [Param],
    /// Changes the checkout or the remote; needs `--allow-write`.
    write: bool,
    /// Exit codes that are answers rather than failures.
    ok_codes: &'static [i32],
    /// Cut the command's JSON down to what a model needs.
    shape: Option<fn(Value) -> Value>,
    build: fn(&Value) -> std::result::Result<Invocation, String>,
}

/// A string argument. The value becomes a command-line argument, so one that
/// starts with `-` is refused: a "branch" of `--force` must not become a flag.
fn string(args: &Value, name: &str) -> std::result::Result<Option<String>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            if value.is_empty() || value.starts_with('-') || value.contains('\0') {
                Err(format!("{name} is not a valid value: {value:?}"))
            } else {
                Ok(Some(value.clone()))
            }
        }
        Some(_) => Err(format!("{name} must be a string")),
    }
}

fn number(args: &Value, name: &str) -> std::result::Result<Option<u64>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{name} must be a non-negative integer")),
    }
}

fn boolean(args: &Value, name: &str) -> std::result::Result<Option<bool>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("{name} must be true or false")),
    }
}

fn numeric(args: &Value, name: &str) -> std::result::Result<Option<String>, String> {
    Ok(number(args, name)?.map(|value| value.to_string()))
}

fn require<T>(value: Option<T>, name: &str) -> std::result::Result<T, String> {
    value.ok_or_else(|| format!("{name} is required"))
}

const JSON_OK: &[i32] = &[0];

/// The fields of a triage row that decide what to do with the branch. Each row
/// otherwise repeats kilobytes of evidence, which over hundreds of branches is
/// more than a model can hold.
const TRIAGE_FIELDS: [&str; 7] = [
    "branch",
    "recommended_action",
    "reason",
    "confidence",
    "merge_allowed",
    "close_allowed",
    "next_detail_command",
];

/// Keep the rows that were analyzed, and count all of them by action.
fn shape_triage(mut triage: Value) -> Value {
    let rows = match triage.get_mut("rows").map(Value::take) {
        Some(Value::Array(rows)) => rows,
        _ => return triage,
    };
    let mut actions = serde_json::Map::new();
    let mut analyzed = Vec::new();
    for row in rows {
        let action = row["recommended_action"].as_str().unwrap_or("unknown");
        let count = actions.entry(action).or_insert(json!(0));
        *count = json!(count.as_u64().unwrap_or(0) + 1);
        if action != "unknown" {
            let kept: serde_json::Map<String, Value> = TRIAGE_FIELDS
                .iter()
                .filter_map(|field| Some((field.to_string(), row.get(*field)?.clone())))
                .collect();
            analyzed.push(Value::Object(kept));
        }
    }
    triage["actions"] = Value::Object(actions);
    triage["rows"] = Value::Array(analyzed);
    triage
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "oak_agent_state",
        description: "The repo's current state in one document, with the next useful commands. \
                      Start here when unsure.",
        params: &[],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |_| Ok(Invocation::new(&["agent", "state", "--json", "--compact"])),
    },
    Tool {
        name: "oak_status",
        description: "Changed files, the current branch and its description.",
        params: &[],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |_| Ok(Invocation::new(&["status", "--json"])),
    },
    Tool {
        name: "oak_diff",
        description: "What a branch contributes since it forked from main, without checking it \
                      out. Without a branch, the current branch including uncommitted edits. The \
                      branch must be known locally; oak_branch_review reads remote ones.",
        params: &[
            param(
                "branch",
                "string",
                "Branch to diff (default: the current branch)",
            ),
            param(
                "hunks",
                "boolean",
                "Include patches, not just the per-file summary",
            ),
            param(
                "max_bytes",
                "integer",
                "Bound on patch bytes when hunks is true",
            ),
        ],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |args| {
            let invocation = match string(args, "branch")? {
                Some(branch) => Invocation::new(&["diff"]).arg(branch),
                None => Invocation::new(&["diff", "--branch"]),
            }
            .arg("--json");
            if boolean(args, "hunks")?.unwrap_or(false) {
                let max_bytes = number(args, "max_bytes")?.unwrap_or(DEFAULT_DIFF_BYTES);
                Ok(invocation
                    .arg("--hunks")
                    .option("--max-bytes", Some(max_bytes.to_string())))
            } else {
                Ok(invocation)
            }
        },
    },
    Tool {
        name: "oak_log",
        description: "Recent history of a branch on the remote, main by default.",
        params: &[
            param("branch", "string", "Branch to walk (default: main)"),
            param("limit", "integer", "How many commits (default: 20)"),
        ],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |args| {
            let limit = number(args, "limit")?.unwrap_or(20).to_string();
            Ok(Invocation::new(&["log", "--remote", "--json", "-n"])
                .arg(limit)
                .option("--branch", string(args, "branch")?))
        },
    },
    Tool {
        name: "oak_branch_list",
        description: "Branches on the remote.",
        params: &[param(
            "status",
            "string",
            "Branch status to list (default: open)",
        )],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |args| {
            let status = string(args, "status")?.unwrap_or_else(|| "open".to_string());
            Ok(Invocation::new(&["branch", "list", "--remote", "--json"])
                .option("--status", Some(status)))
        },
    },
    Tool {
        name: "oak_branch_triage",
        description: "Sort open branches without checking them out: which can land, which \
                      produced nothing new and can close, and which need a reader. Lists the \
                      branches it analyzed and counts the rest; oak_branch_review has the \
                      evidence for one.",
        params: &[
            param(
                "only",
                "string",
                "Only one bucket: mergeable, closable or ambiguous",
            ),
            param(
                "status",
                "string",
                "Branch status to triage (default: open)",
            ),
            param("limit", "integer", "Maximum branches to analyze in depth"),
        ],
        write: false,
        ok_codes: JSON_OK,
        shape: Some(shape_triage),
        build: |args| {
            let status = string(args, "status")?.unwrap_or_else(|| "open".to_string());
            Ok(Invocation::new(&["branch", "triage", "--remote", "--json"])
                .option("--status", Some(status))
                .option("--only", string(args, "only")?)
                .option("--limit", numeric(args, "limit")?))
        },
    },
    Tool {
        name: "oak_branch_review",
        description: "The evidence for one branch: what it changed, what it's based on, and \
                      whether it would merge safely.",
        params: &[required("branch", "string", "Branch to review")],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |args| {
            let branch = require(string(args, "branch")?, "branch")?;
            Ok(Invocation::new(&["branch", "review"])
                .arg(branch)
                .arg("--remote")
                .arg("--merge-preview")
                .arg("--json"))
        },
    },
    Tool {
        name: "oak_ci_status",
        description: "Each CI service (workflow file) on main: passing, failing since a commit, or running. CI runs on main after merges.",
        params: &[],
        write: false,
        // 1 is failed or no runs, 3 is still running: both are the answer.
        ok_codes: &[0, 1, 3],
        shape: None,
        build: |_| Ok(Invocation::new(&["ci", "status", "--json"])),
    },
    Tool {
        name: "oak_ci_runs",
        description: "Recent CI runs for this repo, newest first.",
        params: &[param("limit", "integer", "How many runs (default: 20)")],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |args| {
            Ok(Invocation::new(&["ci", "runs", "--json"])
                .option("--limit", numeric(args, "limit")?))
        },
    },
    Tool {
        name: "oak_ci_logs",
        description: "The logs of one CI run, failed steps only by default.",
        params: &[
            required("run_id", "integer", "The run"),
            param("all_steps", "boolean", "Include steps that passed"),
            param("max_bytes", "integer", "Bound on log bytes"),
        ],
        write: false,
        ok_codes: JSON_OK,
        shape: None,
        build: |args| {
            let run_id = require(number(args, "run_id")?, "run_id")?;
            let max_bytes = number(args, "max_bytes")?.unwrap_or(DEFAULT_LOG_BYTES);
            Ok(Invocation::new(&["ci", "logs"])
                .arg(run_id.to_string())
                .arg("--json")
                .flag("--failed", !boolean(args, "all_steps")?.unwrap_or(false))
                .option("--max-bytes", Some(max_bytes.to_string())))
        },
    },
    Tool {
        name: "oak_commit",
        description: "Checkpoint every change in the working tree locally. Takes no message: \
                      the branch description says what the work is.",
        params: &[],
        write: true,
        ok_codes: JSON_OK,
        shape: None,
        build: |_| Ok(Invocation::new(&["commit", "--json"])),
    },
    Tool {
        name: "oak_desc",
        description: "Set the current branch's description, which becomes the squash-merge \
                      message.",
        params: &[
            required("description", "string", "What this branch does and why"),
            param(
                "append",
                "boolean",
                "Add to the description instead of replacing it",
            ),
        ],
        write: true,
        ok_codes: JSON_OK,
        shape: None,
        // The text goes over stdin, so it may start with `-` and isn't limited
        // by argv.
        build: |args| {
            let description = match args.get("description") {
                Some(Value::String(text)) if !text.trim().is_empty() => text.clone(),
                _ => return Err("description is required".to_string()),
            };
            let invocation = Invocation::new(&["desc", "--file", "-", "--json"])
                .flag("--append", boolean(args, "append")?.unwrap_or(false));
            Ok(Invocation {
                stdin: Some(description),
                ..invocation
            })
        },
    },
    Tool {
        name: "oak_push",
        description: "Publish the current branch to the remote so it can be reviewed.",
        params: &[],
        write: true,
        ok_codes: JSON_OK,
        shape: None,
        build: |_| Ok(Invocation::new(&["push", "--json"])),
    },
];

fn tool_json(tool: &Tool) -> Value {
    let mut properties = serde_json::Map::new();
    for param in tool.params {
        properties.insert(
            param.name.to_string(),
            json!({ "type": param.kind, "description": param.description }),
        );
    }
    let required: Vec<&str> = tool
        .params
        .iter()
        .filter(|param| param.required)
        .map(|param| param.name)
        .collect();
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": !tool.write, "openWorldHint": true },
    })
}

/// One side of a child's output: the bytes kept, and whether there were more.
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Keep the first [`MAX_CAPTURE_BYTES`], then drain the rest so the child can't
/// block on a full pipe.
fn capture(mut stream: impl Read) -> Captured {
    let mut bytes = Vec::new();
    let limit = MAX_CAPTURE_BYTES as u64;
    let _ = stream.by_ref().take(limit).read_to_end(&mut bytes);
    let rest = io::copy(&mut stream, &mut io::sink()).unwrap_or(0);
    Captured {
        bytes,
        truncated: rest > 0,
    }
}

/// `text`, cut to what a model should get, saying so if anything was lost.
fn bounded_text(mut text: String, truncated: bool) -> String {
    let over = text.len() > MAX_OUTPUT_BYTES;
    if over {
        let mut end = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    if over || truncated {
        text.push_str(&format!(
            "\n[output truncated at {MAX_OUTPUT_BYTES} bytes; ask for less]"
        ));
    }
    text
}

fn captured_text(captured: &Captured) -> String {
    let text = String::from_utf8_lossy(&captured.bytes)
        .trim_end()
        .to_string();
    bounded_text(text, captured.truncated)
}

/// The text of a successful call: shaped when the tool shapes its output and
/// all of it parsed, otherwise as it came.
fn success_text(tool: &Tool, captured: &Captured) -> String {
    let shaped = tool
        .shape
        .filter(|_| !captured.truncated)
        .and_then(|shape| {
            let value = serde_json::from_slice(&captured.bytes).ok()?;
            Some(shape(value).to_string())
        });
    match shaped {
        Some(text) => bounded_text(text, false),
        None => captured_text(captured),
    }
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Run `oak` and turn what happened into a tool result. Anything that goes
/// wrong here is a failed tool call, for the model to read, not a protocol
/// error.
fn run_tool(exe: &Path, tool: &Tool, invocation: Invocation) -> Value {
    let mut command = Command::new(exe);
    command
        .args(&invocation.args)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(if invocation.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return tool_result(format!("could not run oak: {error}"), true),
    };
    let stdin = child.stdin.take();
    let text = invocation.stdin;
    std::thread::spawn(move || {
        if let (Some(mut stdin), Some(text)) = (stdin, text) {
            let _ = stdin.write_all(text.as_bytes());
        }
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout = std::thread::spawn(move || capture(stdout));
    let stderr = std::thread::spawn(move || capture(stderr));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= TOOL_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return tool_result(format!("could not wait for oak: {error}"), true),
        }
    };
    let empty = || Captured {
        bytes: Vec::new(),
        truncated: false,
    };
    let stdout = stdout.join().unwrap_or_else(|_| empty());
    let stderr = stderr.join().unwrap_or_else(|_| empty());

    let Some(status) = status else {
        return tool_result(
            format!("oak timed out after {} seconds", TOOL_TIMEOUT.as_secs()),
            true,
        );
    };
    let code = status.code();
    if code.is_some_and(|code| tool.ok_codes.contains(&code)) {
        return tool_result(success_text(tool, &stdout), false);
    }
    let exit = code.map_or("a signal".to_string(), |code| format!("exit code {code}"));
    let (stdout, stderr) = (captured_text(&stdout), captured_text(&stderr));
    let detail: Vec<&str> = [stdout.as_str(), stderr.as_str()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect();
    tool_result(format!("oak failed ({exit})\n{}", detail.join("\n")), true)
}

struct Server {
    exe: PathBuf,
    allow_write: bool,
}

impl Server {
    fn tools(&self) -> impl Iterator<Item = &'static Tool> + '_ {
        TOOLS.iter().filter(|tool| self.allow_write || !tool.write)
    }

    fn call(&self, params: &Value) -> std::result::Result<Value, (i64, String)> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err((INVALID_PARAMS, "name is required".to_string()));
        };
        let Some(tool) = TOOLS.iter().find(|tool| tool.name == name) else {
            return Err((INVALID_PARAMS, format!("unknown tool: {name}")));
        };
        if tool.write && !self.allow_write {
            return Err((
                INVALID_PARAMS,
                format!("{name} changes the repo; start the server with `oak mcp --allow-write`"),
            ));
        }
        let empty = json!({});
        let arguments = params.get("arguments").unwrap_or(&empty);
        Ok(match (tool.build)(arguments) {
            Ok(invocation) => run_tool(&self.exe, tool, invocation),
            Err(message) => tool_result(message, true),
        })
    }

    fn dispatch(&self, method: &str, params: &Value) -> std::result::Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let version = requested
                    .filter(|version| PROTOCOL_VERSIONS.contains(version))
                    .unwrap_or(PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "oak", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                let tools: Vec<Value> = self.tools().map(tool_json).collect();
                Ok(json!({ "tools": tools }))
            }
            "tools/call" => self.call(params),
            _ => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
        }
    }

    /// Answer one line of input. Notifications and responses get no reply.
    fn handle(&self, line: &str) -> Option<Value> {
        let request: Value = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(error) => return Some(error_reply(&Value::Null, PARSE_ERROR, &error.to_string())),
        };
        let Some(object) = request.as_object() else {
            return Some(error_reply(
                &Value::Null,
                INVALID_REQUEST,
                "expected a JSON-RPC object",
            ));
        };
        let id = object.get("id");
        let Some(method) = object.get("method") else {
            // A reply to something we never sent, or nonsense with an id.
            return match (
                id,
                object.contains_key("result") || object.contains_key("error"),
            ) {
                (Some(id), false) => {
                    Some(error_reply(id, INVALID_REQUEST, "request has no method"))
                }
                _ => None,
            };
        };
        let Some(method) = method.as_str() else {
            let id = id.unwrap_or(&Value::Null);
            return Some(error_reply(id, INVALID_REQUEST, "method must be a string"));
        };
        let id = id?;
        let empty = json!({});
        let params = object.get("params").unwrap_or(&empty);
        Some(match self.dispatch(method, params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_reply(id, code, &message),
        })
    }
}

fn error_reply(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn serve(input: impl BufRead, mut output: impl Write, server: &Server) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle(&line) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

pub fn run(allow_write: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let server = Server { exe, allow_write };
    serve(io::stdin().lock(), io::stdout().lock(), &server)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(name: &str, args: Value) -> std::result::Result<Invocation, String> {
        let tool = TOOLS.iter().find(|tool| tool.name == name).unwrap();
        (tool.build)(&args)
    }

    fn server(allow_write: bool) -> Server {
        Server {
            exe: PathBuf::from("oak-not-used"),
            allow_write,
        }
    }

    fn ask(server: &Server, request: Value) -> Value {
        server.handle(&request.to_string()).expect("a reply")
    }

    #[test]
    fn a_value_cannot_become_a_flag() {
        for branch in ["--force", "-f", ""] {
            let error = build("oak_branch_review", json!({ "branch": branch })).unwrap_err();
            assert!(error.contains("branch"), "{error}");
        }
        assert!(build("oak_log", json!({ "branch": "--all" })).is_err());
    }

    #[test]
    fn wrong_types_are_refused() {
        assert!(build("oak_branch_review", json!({ "branch": 7 })).is_err());
        assert!(build("oak_branch_review", json!({})).is_err());
        assert!(build("oak_ci_logs", json!({ "run_id": "7" })).is_err());
        assert!(build("oak_ci_logs", json!({ "run_id": -1 })).is_err());
        assert!(build("oak_diff", json!({ "hunks": "yes" })).is_err());
    }

    #[test]
    fn tools_build_the_commands_they_wrap() {
        let args = |name, input| build(name, input).unwrap().args.join(" ");
        assert_eq!(args("oak_diff", json!({})), "diff --branch --json");
        assert_eq!(
            args("oak_diff", json!({ "branch": "fix", "hunks": true })),
            "diff fix --json --hunks --max-bytes 200000"
        );
        assert_eq!(
            args("oak_branch_review", json!({ "branch": "fix" })),
            "branch review fix --remote --merge-preview --json"
        );
        assert_eq!(
            args("oak_ci_logs", json!({ "run_id": 7 })),
            "ci logs 7 --json --failed --max-bytes 65536"
        );
        assert_eq!(
            args("oak_ci_logs", json!({ "run_id": 7, "all_steps": true })),
            "ci logs 7 --json --max-bytes 65536"
        );
        assert_eq!(
            args(
                "oak_branch_triage",
                json!({ "only": "mergeable", "limit": 5 })
            ),
            "branch triage --remote --json --status open --only mergeable --limit 5"
        );
    }

    #[test]
    fn a_description_goes_over_stdin_so_it_can_start_with_a_dash() {
        let invocation = build("oak_desc", json!({ "description": "- a list" })).unwrap();
        assert_eq!(invocation.args.join(" "), "desc --file - --json");
        assert_eq!(invocation.stdin.as_deref(), Some("- a list"));
        assert!(build("oak_desc", json!({ "description": "  " })).is_err());
    }

    #[test]
    fn write_tools_are_hidden_and_refused_by_default() {
        let names = |server: &Server| -> Vec<String> {
            ask(
                server,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
            )["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string())
                .collect()
        };
        let read_only = names(&server(false));
        assert!(read_only.contains(&"oak_status".to_string()));
        assert!(!read_only.contains(&"oak_push".to_string()));
        assert!(names(&server(true)).contains(&"oak_push".to_string()));

        let reply = ask(
            &server(false),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "oak_push" },
            }),
        );
        assert_eq!(reply["error"]["code"], INVALID_PARAMS);
        assert!(reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--allow-write"));
    }

    #[test]
    fn nothing_that_lands_or_closes_a_branch_is_exposed() {
        for tool in TOOLS {
            let invocation = (tool.build)(&json!({
                "branch": "x", "run_id": 1, "description": "x",
            }))
            .unwrap();
            for word in ["merge", "close", "reset"] {
                let line = invocation.args.join(" ");
                let safe = line.contains("--merge-preview");
                assert!(safe || !line.contains(word), "{}: {line}", tool.name);
            }
        }
    }

    #[test]
    fn write_tools_are_exactly_commit_describe_and_push() {
        let writes: Vec<_> = TOOLS.iter().filter(|t| t.write).map(|t| t.name).collect();
        assert_eq!(writes, ["oak_commit", "oak_desc", "oak_push"]);
    }

    #[test]
    fn initialize_agrees_a_version() {
        let version = |requested: &str| {
            ask(
                &server(false),
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": requested },
                }),
            )["result"]["protocolVersion"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(version("2024-11-05"), "2024-11-05");
        assert_eq!(version("1999-01-01"), PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn protocol_errors_and_notifications() {
        let server = server(false);
        let parse = server.handle("{not json").unwrap();
        assert_eq!(parse["error"]["code"], PARSE_ERROR);
        assert_eq!(parse["id"], Value::Null);
        assert_eq!(
            server.handle("[1]").unwrap()["error"]["code"],
            INVALID_REQUEST
        );
        assert_eq!(
            ask(
                &server,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "nope" })
            )["error"]["code"],
            METHOD_NOT_FOUND
        );
        assert_eq!(
            ask(
                &server,
                json!({ "jsonrpc": "2.0", "id": "a", "method": "ping" })
            )["id"],
            "a"
        );
        assert!(server
            .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
        assert!(server
            .handle(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#)
            .is_none());
    }

    #[test]
    fn long_output_is_cut_and_says_so() {
        let captured = capture(io::repeat(b'x').take(MAX_CAPTURE_BYTES as u64 + 10));
        assert_eq!(captured.bytes.len(), MAX_CAPTURE_BYTES);
        assert!(captured.truncated);
        assert!(captured_text(&captured).ends_with("ask for less]"));
        let exact = capture(io::repeat(b'x').take(MAX_CAPTURE_BYTES as u64));
        assert!(!exact.truncated);

        // Cut on a character boundary, under the model's limit.
        let text = bounded_text("é".repeat(MAX_OUTPUT_BYTES), false);
        assert!(text.len() < MAX_OUTPUT_BYTES + 100);
        assert!(text.ends_with("ask for less]"));
        assert_eq!(bounded_text("short".to_string(), false), "short");
    }

    #[test]
    fn triage_keeps_decisions_and_counts_the_rest() {
        let shaped = shape_triage(json!({
            "branches_deferred": 2,
            "rows": [
                {
                    "branch": "a",
                    "recommended_action": "merge",
                    "merge_allowed": true,
                    "missing_data": ["x".repeat(4096)],
                    "checks": { "ci": "passed" },
                },
                { "branch": "b", "recommended_action": "unknown" },
                { "branch": "c", "recommended_action": "unknown" },
            ],
        }));
        assert_eq!(shaped["branches_deferred"], 2);
        assert_eq!(shaped["actions"], json!({ "merge": 1, "unknown": 2 }));
        assert_eq!(
            shaped["rows"],
            json!([{ "branch": "a", "recommended_action": "merge", "merge_allowed": true }])
        );
    }

    #[test]
    fn output_that_is_not_the_expected_json_passes_through() {
        let tool = TOOLS
            .iter()
            .find(|tool| tool.name == "oak_branch_triage")
            .unwrap();
        let captured = Captured {
            bytes: b"not json".to_vec(),
            truncated: false,
        };
        assert_eq!(success_text(tool, &captured), "not json");
        assert_eq!(shape_triage(json!({ "other": 1 })), json!({ "other": 1 }));
    }
}
