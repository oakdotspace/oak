//! `oak pull` / `oak desc --append` / `oak push --plan` checks that need no
//! server: usage errors and local-only appends. The receipts are tested
//! against the real server.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Repository, SqliteRepository};

fn oak(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(cwd)
        .env("HOME", home)
        .env("OAK_API_KEY", "pull-receipts-token")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .args(args)
        .output()
        .unwrap()
}

fn ok(output: Output, label: &str) -> Output {
    assert!(
        output.status.success(),
        "{label} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Stdout must be exactly one JSON document.
fn one_json_document(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let mut stream = serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    let doc = stream
        .next()
        .unwrap_or_else(|| panic!("no JSON on stdout; stderr={}", stderr(output)))
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));
    assert!(
        stream.next().is_none(),
        "stdout must carry exactly one JSON document: {stdout}"
    );
    doc
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn desc_append_without_remote_appends_locally() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let checkout = temp.path().join("c");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    ok(oak(&home, &checkout, &["init", "."]), "init");
    ok(oak(&home, &checkout, &["desc", "one"]), "desc");
    let json = one_json_document(&ok(
        oak(&home, &checkout, &["desc", "--append", "two", "--json"]),
        "append",
    ));
    assert_eq!(json["description"], "one\n\ntwo");
    assert_eq!(json["remote_synced"], "not_linked");
    assert_eq!(json["append"]["base"], "not_linked");

    // An empty append is a usage error, not a silent no-op.
    let empty = oak(&home, &checkout, &["desc", "--append", "  ", "--json"]);
    assert_eq!(empty.status.code(), Some(2));
}

#[test]
fn pull_branch_only_rejects_abort_and_json_continue_reports_one_document() {
    let temp = tempfile::tempdir().unwrap();
    let output = oak(
        temp.path(),
        temp.path(),
        &["pull", "--branch-only", "--abort"],
    );
    assert_eq!(output.status.code(), Some(2));
    // `--json --continue` is accepted now; outside a repository it fails
    // with the standard envelope instead of a usage error.
    let output = oak(temp.path(), temp.path(), &["pull", "--json", "--continue"]);
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["error"]["code"], "repo_not_found");
}

#[test]
fn push_plan_requires_json_and_a_linked_repository() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let checkout = temp.path().join("c");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    let usage = oak(&home, &checkout, &["push", "--plan"]);
    assert_eq!(usage.status.code(), Some(2));

    ok(oak(&home, &checkout, &["init", "."]), "init");
    std::fs::write(checkout.join("f.txt"), "x\n").unwrap();
    ok(oak(&home, &checkout, &["commit"]), "commit");
    let output = oak(
        &home,
        &checkout,
        &["push", "--plan", "--json", "-r", "http://127.0.0.1:9"],
    );
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    let json = one_json_document(&output);
    assert_eq!(json["error"]["code"], "invalid_argument");
    // The plan never persisted the remote it was given.
    let repo = SqliteRepository::open(&checkout.join(".oak/oak.db")).unwrap();
    assert!(repo
        .get_metadata(oak_core::MetadataKey::RemoteUrl)
        .unwrap()
        .is_none());
}

/// QA-W3a L2: the previous description is preserved byte for byte.
#[test]
fn desc_append_preserves_previous_bytes_exactly() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let checkout = temp.path().join("c");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    ok(oak(&home, &checkout, &["init", "."]), "init");
    let previous = "line one\r\nünïcode\ttrailing\t\n\n";
    let file = temp.path().join("prev.txt");
    std::fs::write(&file, previous).unwrap();
    ok(
        oak(
            &home,
            &checkout,
            &["desc", "--file", file.to_str().unwrap()],
        ),
        "desc",
    );
    let json = one_json_document(&ok(
        oak(&home, &checkout, &["desc", "--append", "added", "--json"]),
        "append",
    ));
    assert_eq!(json["append"]["previous_description"], previous);
    assert_eq!(
        json["description"].as_str().unwrap(),
        format!("{previous}\n\nadded")
    );
}
