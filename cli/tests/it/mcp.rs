//! `oak mcp`, driven the way a client does: JSON-RPC lines over the real
//! binary's stdin and stdout, in a real repo.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

fn oak(root: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(root)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    output
}

/// Send `requests`, close stdin, and return the replies in order.
fn session(root: &Path, args: &[&str], requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .arg("mcp")
        .args(args)
        .current_dir(root)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout is only JSON-RPC"))
        .collect()
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn a_client_can_handshake_and_read_the_repo() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    oak(root, &["init"]);
    std::fs::write(root.join("new.txt"), "hello\n").unwrap();

    let replies = session(
        root,
        &[],
        &[
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1" },
                },
            }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            call(3, "oak_status", json!({})),
            call(4, "oak_branch_review", json!({ "branch": "--force" })),
        ],
    );
    assert_eq!(replies.len(), 4, "{replies:?}");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "oak");
    assert!(replies[1]["result"]["tools"].as_array().unwrap().len() >= 12);

    assert_eq!(replies[2]["result"]["isError"], false, "{:?}", replies[2]);
    let status: Value = serde_json::from_str(text(&replies[2])).unwrap();
    assert!(
        status["changes"].to_string().contains("new.txt"),
        "{status}"
    );

    assert_eq!(replies[3]["result"]["isError"], true);
    assert!(text(&replies[3]).contains("branch"));
}

#[test]
fn a_failed_command_is_a_tool_error_with_its_output() {
    let temp = tempfile::tempdir().unwrap();
    let replies = session(temp.path(), &[], &[call(1, "oak_status", json!({}))]);
    assert_eq!(replies[0]["result"]["isError"], true, "{:?}", replies[0]);
    assert!(text(&replies[0]).starts_with("oak failed (exit code"));
}

#[test]
fn writing_needs_the_flag_and_then_works() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    oak(root, &["init"]);
    std::fs::write(root.join("new.txt"), "hello\n").unwrap();

    let refused = session(root, &[], &[call(1, "oak_commit", json!({}))]);
    assert!(refused[0]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("--allow-write"));
    let clean: Value = serde_json::from_slice(&oak(root, &["status", "--json"]).stdout).unwrap();
    assert!(clean["changes"].to_string().contains("new.txt"));

    let replies = session(
        root,
        &["--allow-write"],
        &[
            call(1, "oak_commit", json!({})),
            call(
                2,
                "oak_desc",
                json!({ "description": "- starts with a dash" }),
            ),
        ],
    );
    assert_eq!(replies[0]["result"]["isError"], false, "{:?}", replies[0]);
    assert_eq!(replies[1]["result"]["isError"], false, "{:?}", replies[1]);
    let status: Value = serde_json::from_slice(&oak(root, &["status", "--json"]).stdout).unwrap();
    assert_eq!(status["branch_description"], "- starts with a dash");
    assert!(
        !status["changes"].to_string().contains("new.txt"),
        "{status}"
    );
}
