//! Machine-contract harness, process level.
//!
//! Walks every runnable command listed in `contract_surface.tsv` (kept equal
//! to the real clap tree by the binary's `contract_surface_matches_the_
//! golden_file` unit test) and asserts, against the built `oak` binary, the
//! parts of the contract that hold for every command by construction:
//!
//! * a usage error under `--json` writes exactly one JSON document to stdout
//!   — `error.code == "invalid_argument"` — and exits 2, whether or not the
//!   command itself has a `--json` flag;
//! * the same usage error without `--json` writes nothing to stdout, reports
//!   on stderr, and exits 2;
//! * every command without `--json` is on the short, reasoned exempt list.
//!
//! # Extension point: fault matrix (workstream F2)
//!
//! [`for_each_surface_command`] is the walk; [`assert_one_error_document`]
//! is the document check. The remote fault matrix — every remote-capable
//! command against a mock server answering {connection refused, 401, 403,
//! 404, 409, 429 + Retry-After, 500} must produce one document, the table's
//! exit code, and a classified code (never `remote_error` for a classified
//! fault) — plugs in as further tests in this file built from those two.

use std::process::{Command, Output};

const SURFACE: &str = include_str!("contract_surface.tsv");

/// A flag no command defines; forces a clap usage error.
const UNKNOWN_FLAG: &str = "--contract-harness-unknown-flag";

pub(crate) struct SurfaceCommand {
    pub path: Vec<&'static str>,
    pub accepts_json: bool,
    pub exempt_reason: &'static str,
}

pub(crate) fn surface() -> Vec<SurfaceCommand> {
    SURFACE
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let path = fields.next().unwrap().split(' ').collect();
            let mode = fields.next().expect("mode column");
            SurfaceCommand {
                path,
                accepts_json: mode.starts_with("json"),
                exempt_reason: fields.next().unwrap_or_default(),
            }
        })
        .collect()
}

pub(crate) fn for_each_surface_command(mut visit: impl FnMut(&SurfaceCommand)) {
    let commands = surface();
    assert!(
        commands.len() > 100,
        "surface file lists only {} commands",
        commands.len()
    );
    for command in &commands {
        visit(command);
    }
}

/// An empty HOME, so no stored credential or global config leaks in.
fn isolated_home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

fn oak(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("HOME", isolated_home())
        .env("OAK_AUTHOR", "contract")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_NO_AUTO_UPDATE", "1")
        .env("OAK_WATCH", "0")
        .env("NO_COLOR", "1")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// Stdout is exactly one JSON document that is an error envelope; returns
/// its `error` object. `context` names the invocation in failures.
pub(crate) fn assert_one_error_document(output: &Output, context: &str) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut documents =
        serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    let document = match documents.next() {
        Some(Ok(document)) => document,
        other => panic!("{context}: stdout is not a JSON document ({other:?}): {stdout:?}"),
    };
    assert!(
        documents.next().is_none(),
        "{context}: more than one JSON document on stdout: {stdout:?}"
    );
    let error = document
        .get("error")
        .unwrap_or_else(|| panic!("{context}: no `error` object: {stdout}"))
        .clone();
    let code = error["code"].as_str().unwrap_or_default();
    assert!(
        !code.is_empty() && !matches!(code, "error" | "generic" | "unknown"),
        "{context}: error.code is {code:?}"
    );
    assert!(error["message"].is_string(), "{context}: no error.message");
    assert!(
        error["recommended_next_commands"].is_array(),
        "{context}: no error.recommended_next_commands"
    );
    assert_eq!(document["schema_version"], 1, "{context}");
    error
}

#[test]
fn usage_error_under_json_is_one_invalid_argument_document_for_every_command() {
    let dir = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for_each_surface_command(|command| {
        let mut args = command.path.clone();
        args.extend([UNKNOWN_FLAG, "--json"]);
        let context = format!("oak {}", args.join(" "));
        let output = oak(dir.path(), &args);
        let outcome = std::panic::catch_unwind(|| {
            assert_eq!(output.status.code(), Some(2), "{context}: exit code");
            let error = assert_one_error_document(&output, &context);
            assert_eq!(error["code"], "invalid_argument", "{context}");
            assert_eq!(error["retryable"], false, "{context}");
            assert!(
                error["message"].as_str().unwrap().contains(UNKNOWN_FLAG),
                "{context}: message does not name the bad flag: {error}"
            );
            assert!(
                output.stderr.is_empty(),
                "{context}: stderr under --json: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        });
        if let Err(panic) = outcome {
            failures.push(
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .unwrap_or_else(|| context.clone()),
            );
        }
    });
    assert!(
        failures.is_empty(),
        "{} command(s) break the usage-error contract:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn usage_error_without_json_stays_on_stderr_for_every_command() {
    let dir = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for_each_surface_command(|command| {
        let mut args = command.path.clone();
        args.push(UNKNOWN_FLAG);
        let context = format!("oak {}", args.join(" "));
        let output = oak(dir.path(), &args);
        if output.status.code() != Some(2)
            || !output.stdout.is_empty()
            || !String::from_utf8_lossy(&output.stderr).contains(UNKNOWN_FLAG)
        {
            failures.push(format!(
                "{context}: exit {:?}, stdout {:?}, stderr {:?}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    });
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The commands that have no `--json`, pinned. Adding one here is a
/// reviewed decision; the reason lives next to the command in
/// `contract_surface.tsv`. Agent-facing commands do not belong on it.
const EXEMPT: &[&str] = &[
    "archive",
    "branch rename",
    "checkout",
    "completions",
    "hash",
    "login",
    "logout",
    "maintenance compact",
    "mcp",
    "mount",
    "mount __fskit-broker",
    "mount __fskit-broker-ping",
    "mount __resume",
    "mount __serve",
    "mount end",
    "mount forget",
    "mount worktree-create",
    "mount worktree-remove",
    "rev-parse",
    "skill install",
    "upgrade",
    "watch hook",
];

#[test]
fn commands_without_json_are_exactly_the_reasoned_exempt_list() {
    let mut exempt = Vec::new();
    for_each_surface_command(|command| {
        if !command.accepts_json {
            assert!(
                command.exempt_reason.trim().len() >= 10,
                "oak {} has no --json and no written reason",
                command.path.join(" ")
            );
            exempt.push(command.path.join(" "));
        }
    });
    let pinned: Vec<String> = EXEMPT.iter().map(|path| path.to_string()).collect();
    assert_eq!(
        exempt, pinned,
        "the set of commands without --json changed; give the command --json or review the exemption"
    );
    // The commands agents drive must never be exempt.
    for required in [
        "status",
        "switch",
        "fetch",
        "pull",
        "push",
        "commit",
        "merge",
        "start",
        "finish",
        "reset",
        "restore",
        "whoami",
        "clone",
        "log",
        "diff",
        "desc",
        "close",
        "info",
        "sparse",
        "sparse list",
        "sparse set",
        "sparse add",
        "sparse remove",
        "sparse disable",
        "agent state",
        "agent publication forget",
        "conflict status",
        "branch list",
    ] {
        let command = surface()
            .into_iter()
            .find(|command| command.path.join(" ") == required)
            .unwrap_or_else(|| panic!("`oak {required}` is not on the surface"));
        assert!(command.accepts_json, "`oak {required}` must accept --json");
    }
}

// ---------------------------------------------------------------------------
// Receipts and failures of the commands that gained `--json`
// ---------------------------------------------------------------------------

fn json_stdout(output: &Output, context: &str) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut documents =
        serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    let document = documents
        .next()
        .and_then(Result::ok)
        .unwrap_or_else(|| panic!("{context}: stdout is not JSON: {stdout:?}"));
    assert!(
        documents.next().is_none(),
        "{context}: more than one document"
    );
    document
}

fn repo_with_commit() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(oak(dir.path(), &["init", "."]).status.success());
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    assert!(oak(dir.path(), &["commit"]).status.success());
    dir
}

#[test]
fn commands_that_gained_json_fail_with_one_specific_document() {
    let outside = tempfile::tempdir().unwrap();
    for args in [
        &["switch", "main", "--json"][..],
        &["fetch", "--json"],
        &["reset", "--force", "--json"],
        &["restore", "--force", "--json"],
        &["pull", "--continue", "--json"],
        &["status", "--reconcile", "--json"],
        &["sparse", "add", "src", "--json"],
        &["sparse", "disable", "--json"],
    ] {
        let context = format!("oak {} (outside a repository)", args.join(" "));
        let output = oak(outside.path(), args);
        assert_eq!(output.status.code(), Some(1), "{context}");
        let error = assert_one_error_document(&output, &context);
        assert_eq!(error["code"], "repo_not_found", "{context}");
    }

    let repo = repo_with_commit();
    for (args, code, exit) in [
        (
            &["switch", "no-such-branch", "--json"][..],
            "branch_not_found",
            1,
        ),
        (&["switch", "--json"], "invalid_argument", 2),
        (&["switch", "main", "--json"], "branch_is_server_only", 1),
        (&["pull", "--continue", "--json"], "no_sync_in_progress", 1),
        (&["pull", "--abort", "--json"], "no_sync_in_progress", 1),
        (&["reset", "--json"], "invalid_argument", 2),
        (&["restore", "--json"], "invalid_argument", 2),
        (&["merge", "--abort", "--json"], "no_merge_in_progress", 1),
        (
            &[
                "agent",
                "publication",
                "forget",
                "no-such-operation",
                "--json",
            ],
            "invalid_argument",
            2,
        ),
    ] {
        let context = format!("oak {}", args.join(" "));
        let output = oak(repo.path(), args);
        let error = assert_one_error_document(&output, &context);
        assert_eq!(error["code"], code, "{context}: {error}");
        assert_eq!(output.status.code(), Some(exit), "{context}");
    }

    // No stored credential: an auth failure naming the login command, not
    // a remote error (nothing was sent anywhere).
    let output = oak(
        repo.path(),
        &["whoami", "--remote", "http://127.0.0.1:9", "--json"],
    );
    assert_eq!(output.status.code(), Some(6));
    let error = assert_one_error_document(&output, "oak whoami --json");
    assert_eq!(error["code"], "auth_required");
    assert_eq!(
        error["recommended_next_commands"],
        serde_json::json!(["oak login --remote=http://127.0.0.1:9"])
    );

    // `oak close` with no name outside a repository is "not a repository".
    let output = oak(outside.path(), &["close", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        assert_one_error_document(&output, "oak close --json")["code"],
        "repo_not_found"
    );

    // `--json=true` is not a valid spelling, but it asks for JSON: the
    // usage error it causes is a document.
    let output = oak(repo.path(), &["status", "--json=true"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        assert_one_error_document(&output, "oak status --json=true")["code"],
        "invalid_argument"
    );

    // A command (not the parser) refusing its arguments claims nothing about
    // retrying: `invalid_argument` is also raised for conditions that change.
    let output = oak(repo.path(), &["reset", "--json"]);
    let error = assert_one_error_document(&output, "oak reset --json");
    assert!(error.get("retryable").is_none(), "{error}");
}

#[test]
fn commands_that_gained_json_print_one_truthful_receipt() {
    let repo = repo_with_commit();
    let head = String::from_utf8(oak(repo.path(), &["hash"]).stdout).unwrap();
    let head = head.trim();

    std::fs::write(repo.path().join("a.txt"), "dirty\n").unwrap();
    let output = oak(repo.path(), &["reset", "--force", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let receipt = json_stdout(&output, "oak reset --force --json");
    assert_eq!(receipt["kind"], "reset");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["changed"], true);
    assert_eq!(receipt["reverted"], serde_json::json!(["a.txt"]));
    assert_eq!(receipt["deleted"], serde_json::json!([]));
    assert_eq!(receipt["reverted_count"], 1);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
        "one\n",
        "the receipt must describe a reset that happened"
    );

    std::fs::write(repo.path().join("a.txt"), "dirty again\n").unwrap();
    let output = oak(repo.path(), &["restore", "a.txt", "--force", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let receipt = json_stdout(&output, "oak restore --force --json");
    assert_eq!(receipt["kind"], "restore");
    assert_eq!(receipt["changed"], true);
    assert_eq!(receipt["reverted"], serde_json::json!(["a.txt"]));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
        "one\n"
    );

    // Nothing to do: the receipt says so instead of echoing the argument.
    for args in [
        &["restore", "a.txt", "--force", "--json"][..],
        &["reset", "a.txt", "--force", "--json"],
        &["reset", "--force", "--json"],
    ] {
        let output = oak(repo.path(), args);
        assert!(output.status.success(), "{output:?}");
        let receipt = json_stdout(&output, &args.join(" "));
        assert_eq!(receipt["changed"], false, "{args:?}");
        assert_eq!(receipt["reverted"], serde_json::json!([]), "{args:?}");
        assert_eq!(receipt["deleted"], serde_json::json!([]), "{args:?}");
        assert!(receipt.get("completed").is_none() && receipt.get("path").is_none());
    }

    // An untracked file is deleted, and reported as deleted.
    std::fs::write(repo.path().join("scratch.txt"), "x\n").unwrap();
    let receipt = json_stdout(
        &oak(repo.path(), &["reset", "--force", "--json"]),
        "reset untracked",
    );
    assert_eq!(receipt["deleted"], serde_json::json!(["scratch.txt"]));
    assert!(!repo.path().join("scratch.txt").exists());

    let before = json_stdout(&oak(repo.path(), &["status", "--json"]), "status");
    let output = oak(repo.path(), &["switch", "-c", "contract-b", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let receipt = json_stdout(&output, "oak switch -c --json");
    assert_eq!(receipt["kind"], "switch");
    assert_eq!(receipt["branch"], "contract-b");
    assert_eq!(receipt["previous_branch"], before["branch"]);
    assert_eq!(receipt["previous_head"], head);
    let after = json_stdout(&oak(repo.path(), &["status", "--json"]), "status");
    assert_eq!(after["branch"], "contract-b");
    assert_eq!(receipt["head"], after["head"]);
    for command in receipt["recommended_next_commands"].as_array().unwrap() {
        let args: Vec<&str> = command.as_str().unwrap().split(' ').skip(1).collect();
        assert!(oak(repo.path(), &args).status.success(), "{command}");
    }

    let output = oak(repo.path(), &["status", "--reconcile", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        json_stdout(&output, "status --reconcile --json")["branch"],
        "contract-b"
    );

    let output = oak(repo.path(), &["sparse", "set", "a.txt", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let scope = json_stdout(&output, "oak sparse set --json");
    let listed = json_stdout(
        &oak(repo.path(), &["sparse", "list", "--json"]),
        "sparse list",
    );
    assert_eq!(scope, listed, "the receipt is the stored scope");
}

#[test]
fn init_and_split_print_one_receipt_of_what_they_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let output = oak(dir.path(), &["init", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let receipt = json_stdout(&output, "oak init --json");
    assert_eq!(receipt["kind"], "init");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["imported_commit_count"], 0);
    let status = json_stdout(&oak(dir.path(), &["status", "--json"]), "status");
    assert_eq!(receipt["branch"], status["branch"]);
    assert!(dir.path().join(".oak").is_dir());

    // A second init fails with one specific document.
    let output = oak(dir.path(), &["init", "--json"]);
    assert_eq!(
        assert_one_error_document(&output, "oak init --json (again)")["code"],
        "repo_already_exists"
    );

    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    assert!(oak(dir.path(), &["commit"]).status.success());
    std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
    assert!(oak(dir.path(), &["commit"]).status.success());
    let log = json_stdout(&oak(dir.path(), &["log", "--json"]), "log");
    let commits: Vec<String> = log
        .as_array()
        .or_else(|| log["commits"].as_array())
        .expect("log entries")
        .iter()
        .map(|entry| entry["hash"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(commits.len(), 2, "{log}");
    // Oldest first: keep the first commit, move the second to a new branch.
    // The plan lives outside the repository so the tree stays clean.
    let plans = tempfile::tempdir().unwrap();
    let plan_path = plans.path().join("plan.txt");
    std::fs::write(
        &plan_path,
        format!(
            "pick {}\nsplit contract-split\npick {}\n",
            commits[1], commits[0]
        ),
    )
    .unwrap();
    let plan_arg = plan_path.to_str().unwrap();

    let output = oak(
        dir.path(),
        &["split", "--plan", plan_arg, "--dry-run", "--json"],
    );
    let dry = json_stdout(&output, "oak split --dry-run --json");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(dry["kind"], "split");
    assert_eq!(dry["written"], false);
    assert_eq!(dry["branches"], serde_json::json!([]));

    let output = oak(dir.path(), &["split", "--plan", plan_arg, "--json"]);
    assert!(output.status.success(), "{output:?}");
    let receipt = json_stdout(&output, "oak split --json");
    assert_eq!(receipt["written"], true);
    let branches = receipt["branches"].as_array().unwrap();
    assert_eq!(branches.len(), 2, "{receipt}");
    assert!(branches.iter().any(|b| b["name"] == "contract-split"));
    // Each reported head is that branch's head now.
    for branch in branches {
        let shown = json_stdout(
            &oak(
                dir.path(),
                &["branch", "show", branch["name"].as_str().unwrap(), "--json"],
            ),
            "branch show",
        );
        assert_eq!(shown["head"], branch["head"], "{shown}");
    }

    // `--json` without `--plan` is a usage error document (the editor cannot run).
    let output = oak(dir.path(), &["split", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        assert_one_error_document(&output, "oak split --json")["code"],
        "invalid_argument"
    );
}

/// The first push of a repository that is not linked to a remote, with no
/// terminal for the org picker: the advice names the command that links it.
#[test]
fn first_push_of_an_unlinked_repo_recommends_the_linking_command() {
    let repo = repo_with_commit();
    let output = oak(repo.path(), &["push", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error = assert_one_error_document(&output, "oak push --json (unlinked)");
    assert_eq!(error["code"], "configuration_error");
    assert_eq!(
        error["recommended_next_commands"],
        serde_json::json!(["oak push --repo <org>/<repo>", "oak info --json"])
    );
}

/// A switch receipt's `previous_head` is the head of the branch that was
/// checked out, as `oak status` reports it — even when the repository-wide
/// head record names another branch's commit (which a parent sync completed
/// for a branch that is no longer checked out leaves behind).
#[test]
fn switch_receipt_reports_the_checked_out_branchs_own_head() {
    use oak_core::{Repository, SqliteRepository};
    let repo = repo_with_commit();
    let first = String::from_utf8(oak(repo.path(), &["hash"]).stdout).unwrap();
    let first = first.trim().to_string();
    std::fs::write(repo.path().join("a.txt"), "two\n").unwrap();
    assert!(oak(repo.path(), &["commit"]).status.success());
    let status = json_stdout(&oak(repo.path(), &["status", "--json"]), "status");
    let branch_head = status["head"].as_str().unwrap().to_string();
    assert_ne!(branch_head, first);
    {
        // Leave the repository-wide head record on a different commit.
        let db = SqliteRepository::open(&repo.path().join(".oak/oak.db")).unwrap();
        db.set_head(&oak_core::Hash(first.clone())).unwrap();
    }
    let status = json_stdout(&oak(repo.path(), &["status", "--json"]), "status");
    assert_eq!(
        status["head"],
        branch_head.as_str(),
        "status reads the branch head"
    );

    let output = oak(repo.path(), &["switch", "-c", "contract-next", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let receipt = json_stdout(&output, "oak switch -c --json");
    assert_eq!(receipt["previous_branch"], status["branch"]);
    assert_eq!(receipt["previous_head"], branch_head.as_str());
    let after = json_stdout(&oak(repo.path(), &["status", "--json"]), "status");
    assert_eq!(receipt["branch"], after["branch"]);
    assert_eq!(receipt["head"], after["head"]);
}
