//! Replays `engine/fixtures/conformance/*.json`, the same files each extension
//! replays through its own binding. Every case's `request` goes through the
//! public entry point and must produce exactly its `response`.
//!
//! `UPDATE_ENGINE_FIXTURES=1 cargo test -p oakvcs-engine --test conformance`
//! rewrites the responses and the header; review that diff, because it is the
//! contract changing.

use std::path::{Path, PathBuf};

use serde_json::Value;

fn updating() -> bool {
    std::env::var_os("UPDATE_ENGINE_FIXTURES").is_some()
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(crate_dir().join("fixtures/conformance"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

fn replay(path: &Path) -> usize {
    let text = std::fs::read_to_string(path).unwrap();
    let mut fixture: Value = serde_json::from_str(&text).unwrap();
    let cases = fixture["cases"].as_array_mut().unwrap();
    let count = cases.len();
    for case in cases {
        let request = case["request"].to_string();
        let response: Value = serde_json::from_str(&oak_engine::call(&request)).unwrap();
        if updating() {
            case["response"] = response;
        } else {
            assert_eq!(
                response,
                case["response"],
                "{}: {}",
                path.display(),
                case["name"]
            );
        }
    }
    if updating() {
        let mut text = serde_json::to_string_pretty(&fixture).unwrap();
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }
    count
}

#[test]
fn fixtures_replay() {
    let files = fixture_files();
    assert!(files.len() >= 3, "fixtures are missing");
    let cases: usize = files.iter().map(|path| replay(path)).sum();
    assert!(cases > 100, "only {cases} cases");
}

#[test]
fn every_action_has_a_captured_or_derived_fixture() {
    let text =
        std::fs::read_to_string(crate_dir().join("fixtures/conformance/cli_parse.json")).unwrap();
    let fixture: Value = serde_json::from_str(&text).unwrap();
    let covered: Vec<&str> = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["request"]["args"]["action"].as_str().unwrap())
        .collect();
    for action in [
        "status",
        "info",
        "diff",
        "log",
        "branch_list",
        "branch_create",
        "branch_switch",
        "branch_close",
        "commit",
        "desc_get",
        "desc_append",
        "fetch",
        "pull",
        "push",
        "merge",
        "restore",
        "whoami",
        "ci_status",
        "ci_runs",
        "ci_logs",
        "ci_publish_preview",
        "ci_publish",
        "conflict_status",
        "version",
    ] {
        assert!(
            covered.contains(&action),
            "no cli.parse fixture for {action}"
        );
    }
}

#[test]
fn header_is_current() {
    let dir = crate_dir();
    let config = cbindgen::Config::from_file(dir.join("cbindgen.toml")).unwrap();
    let mut generated = Vec::new();
    cbindgen::Builder::new()
        .with_crate(&dir)
        .with_config(config)
        .generate()
        .unwrap()
        .write(&mut generated);
    let path = dir.join("include/oak_engine.h");
    if updating() {
        std::fs::write(&path, &generated).unwrap();
    }
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        current,
        String::from_utf8(generated).unwrap(),
        "engine/include/oak_engine.h is stale: rerun with UPDATE_ENGINE_FIXTURES=1"
    );
}
