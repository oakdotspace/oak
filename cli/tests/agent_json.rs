//! Machine-readable output and non-interactive safety contracts.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{
    Branch, ChangeType, Commit, FileChange, FileMode, Manifest, ManifestEntry, MetadataKey,
    Repository, SqliteRepository,
};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn fixture_repo() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    temp
}

fn current_branch(dir: &Path) -> String {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.get_current_branch_name().unwrap().unwrap()
}

fn seed_local_main_from_current_head(dir: &Path) -> oak_core::Hash {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap();
    if repo.get_branch("main").unwrap().is_none() {
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
    }
    repo.set_branch_head("main", &head).unwrap();
    head
}

fn advance_main_with_tracked_txt(
    dir: &Path,
    parent: oak_core::Hash,
    content: &str,
) -> oak_core::Hash {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let parent_commit = repo.get_commit(&parent).unwrap().unwrap();
    let parent_manifest = repo
        .get_manifest(&parent_commit.manifest_hash)
        .unwrap()
        .unwrap();
    let old_entry = parent_manifest.get("tracked.txt").unwrap();
    let new_blob = repo.put_blob(content.as_bytes().to_vec()).unwrap();
    let manifest = Manifest::new(vec![ManifestEntry {
        path: "tracked.txt".to_string(),
        blob_hash: new_blob.clone(),
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "main".to_string(),
        Some(parent),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("main moved".to_string()),
        vec![FileChange {
            path: "tracked.txt".to_string(),
            change_type: ChangeType::Modified,
            old_blob_hash: Some(old_entry.blob_hash.clone()),
            new_blob_hash: Some(new_blob),
            old_path: None,
            old_mode: Some(FileMode::Regular),
            new_mode: Some(FileMode::Regular),
        }],
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    repo.set_branch_head("main", &commit.hash).unwrap();
    commit.hash
}

fn seed_remote_only_feature_commit(dir: &Path, parent: oak_core::Hash) -> oak_core::Hash {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let parent_commit = repo.get_commit(&parent).unwrap().unwrap();
    let parent_manifest = repo
        .get_manifest(&parent_commit.manifest_hash)
        .unwrap()
        .unwrap();
    let old_entry = parent_manifest.get("tracked.txt").unwrap();
    let new_blob = repo.put_blob(b"remote feature\n".to_vec()).unwrap();
    let manifest = Manifest::new(vec![ManifestEntry {
        path: "tracked.txt".to_string(),
        blob_hash: new_blob.clone(),
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "remote-feature".to_string(),
        Some(parent),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("remote branch work".to_string()),
        vec![FileChange {
            path: "tracked.txt".to_string(),
            change_type: ChangeType::Modified,
            old_blob_hash: Some(old_entry.blob_hash.clone()),
            new_blob_hash: Some(new_blob),
            old_path: None,
            old_mode: Some(FileMode::Regular),
            new_mode: Some(FileMode::Regular),
        }],
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    commit.hash
}

async fn mount_remote_branch_list(
    server: &MockServer,
    main_head: &oak_core::Hash,
    feature_head: &oak_core::Hash,
) {
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "branches": [
                {
                    "name": "main",
                    "head": main_head.as_str(),
                    "description": null,
                    "parent_branch": null,
                    "status": "open",
                    "created_at": "2026-01-01T00:00:00Z"
                },
                {
                    "name": "remote-feature",
                    "head": feature_head.as_str(),
                    "description": "Remote feature subject\n\nDetails",
                    "parent_branch": "main",
                    "status": "open",
                    "created_at": "2026-01-02T00:00:00Z"
                },
                {
                    "name": "old-closed",
                    "head": main_head.as_str(),
                    "description": "Already closed",
                    "parent_branch": "main",
                    "status": "closed",
                    "created_at": "2026-01-03T00:00:00Z"
                }
            ]
        })))
        .mount(server)
        .await;
}

fn link_remote(dir: &Path, server: &MockServer) {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
}

fn write_sync_conflict_fixture(dir: &Path) {
    std::fs::write(
        dir.join("tracked.txt"),
        "before\n<<<<<<< tester-branch\nours\n=======\ntheirs\n>>>>>>> main\nafter\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".oak/SYNC_HEAD"),
        "main\ntester-branch\nparenthead\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".oak/SYNC_STATE"),
        r#"{"merged_manifest_hash":"merged123","conflict_paths":["tracked.txt"]}"#,
    )
    .unwrap();
}

fn json_stdout(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
    serde_json::from_str(&stdout(out)).expect("stdout should be valid JSON")
}

#[test]
fn status_json_contains_expected_fields_and_full_description() {
    let temp = fixture_repo();
    let dir = temp.path();
    let desc = "Subject line\nbody one\nbody two";
    assert!(oak(dir, &["desc", desc]).status.success());
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let json = json_stdout(&oak(dir, &["status", "--json"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["branch_description"], desc);
    assert_eq!(json["branch_status"], "open");
    assert_eq!(json["parent"], "main");
    assert_eq!(json["unmerged_commit_count"], 1);
    assert_eq!(json["merge_in_progress"], false);
    assert_eq!(json["sync_in_progress"], false);
    assert_eq!(json["progress_state"]["in_progress"], false);
    assert!(
        json["progress_state"].get("conflict_paths").is_none(),
        "empty conflict_paths should be omitted"
    );
    assert!(
        json["progress_state"].get("next_commands").is_none(),
        "empty next_commands should be omitted"
    );
    assert!(json["branch"].as_str().unwrap().starts_with("tester-"));
    assert!(json["head"].as_str().unwrap().len() >= 40);

    let changes = json["changes"].as_array().unwrap();
    assert!(changes
        .iter()
        .any(|c| c["path"] == "tracked.txt" && c["status"] == "modified"));
    assert!(changes
        .iter()
        .any(|c| c["path"] == "new.txt" && c["status"] == "added"));
}

#[test]
fn status_json_compact_bounds_changes_but_preserves_recall_metadata() {
    let temp = fixture_repo();
    let dir = temp.path();
    for i in 0..25 {
        std::fs::write(dir.join(format!("new-{i:02}.txt")), format!("new {i}\n")).unwrap();
    }

    let full = json_stdout(&oak(dir, &["status", "--json"]));
    assert_eq!(full["changes"].as_array().unwrap().len(), 25);

    let compact = json_stdout(&oak(dir, &["status", "--json", "--compact"]));

    assert_eq!(compact["schema_version"], 1);
    assert_eq!(compact["dirty"], true);
    assert_eq!(compact["change_count"], 25);
    assert_eq!(compact["change_counts"]["added"], 25);
    assert_eq!(compact["change_counts"]["modified"], 0);
    assert_eq!(compact["change_counts"]["deleted"], 0);
    assert_eq!(compact["change_counts"]["renamed"], 0);
    assert_eq!(compact["changes"].as_array().unwrap().len(), 20);
    assert_eq!(compact["changes_omitted"], 5);
    assert_eq!(compact["changes_truncated"], true);
    assert_eq!(compact["recommended_next_commands"][0], "oak status --json");
    assert_eq!(
        compact["recommended_next_commands"][1],
        "oak status --porcelain"
    );
    assert!(
        compact.get("progress_state").is_none(),
        "idle compact status should omit default progress_state"
    );
}

#[test]
fn status_compact_requires_json() {
    let temp = fixture_repo();

    let out = oak(temp.path(), &["status", "--compact"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("required"),
        "stderr should explain the missing --json requirement:\n{}",
        stderr(&out)
    );
}

#[test]
fn info_json_contains_repo_branch_and_progress_metadata() {
    let temp = fixture_repo();
    let dir = temp.path();
    let desc = "Agent-ready summary\n\nMore detail";
    assert!(oak(dir, &["desc", desc]).status.success());

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "agent-json")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.example")
        .unwrap();
    std::fs::write(dir.join(".oak/SYNC_HEAD"), "syncing").unwrap();
    std::fs::write(
        dir.join(".oak/SYNC_STATE"),
        r#"{"merged_manifest_hash":"abc","conflict_paths":["tracked.txt"]}"#,
    )
    .unwrap();

    let json = json_stdout(&oak(dir, &["info", "--json"]));

    assert_eq!(json["schema_version"], 1);
    assert!(json["branch"].as_str().unwrap().starts_with("tester-"));
    assert_eq!(json["branch_description"], desc);
    assert_eq!(json["parent"], "main");
    assert!(json["head"].as_str().unwrap().len() >= 40);
    assert_eq!(json["branch_status"], "open");
    assert_eq!(json["repo_owner"], "oak");
    assert_eq!(json["repo_name"], "agent-json");
    assert_eq!(json["remote_url"], "https://oak.example");
    assert_eq!(json["merge_in_progress"], false);
    assert_eq!(json["sync_in_progress"], true);
    assert_eq!(json["progress_state"]["in_progress"], true);
    assert_eq!(json["progress_state"]["kind"], "sync");
    assert_eq!(json["progress_state"]["conflict_paths"][0], "tracked.txt");
    assert_eq!(
        json["progress_state"]["next_commands"][0],
        "oak pull --continue"
    );
}

#[test]
fn conflict_status_human_succeeds_without_json() {
    let temp = fixture_repo();
    let dir = temp.path();
    write_sync_conflict_fixture(dir);

    let out = oak(dir, &["conflict", "status"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(text.contains("Context: checkout"), "{text}");
    assert!(text.contains("In progress: yes"), "{text}");
    assert!(text.contains("Kind: sync"), "{text}");
    assert!(text.contains("tracked.txt"), "{text}");
    assert!(text.contains("oak pull --continue"), "{text}");
}

#[test]
fn conflict_show_human_succeeds_without_json() {
    let temp = fixture_repo();
    let dir = temp.path();
    write_sync_conflict_fixture(dir);

    let out = oak(dir, &["conflict", "show"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(text.contains("Context: checkout"), "{text}");
    assert!(text.contains("tracked.txt"), "{text}");
    assert!(text.contains("resolution: unresolved"), "{text}");
    assert!(text.contains("conflict markers: yes"), "{text}");
    assert!(text.contains("can take: yes"), "{text}");
}

#[test]
fn conflict_take_warns_on_unbalanced_delimiters() {
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(
        dir.join("tracked.txt"),
        "before\n<<<<<<< tester-branch\nfn open() {\n=======\nfn open() {\n  bar();\n}\n>>>>>>> main\nafter\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".oak/SYNC_HEAD"),
        "main\ntester-branch\nparenthead\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".oak/SYNC_STATE"),
        r#"{"merged_manifest_hash":"merged123","conflict_paths":["tracked.txt"]}"#,
    )
    .unwrap();

    let out = oak(dir, &["conflict", "take", "tracked.txt", "--ours"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("unbalanced delimiters"),
        "expected delimiter warning, got stderr:\n{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("'{}'"),
        "expected brace imbalance detail, got stderr:\n{}",
        stderr(&out)
    );
}

#[test]
fn conflict_take_does_not_warn_on_balanced_delimiters() {
    let temp = fixture_repo();
    let dir = temp.path();
    write_sync_conflict_fixture(dir);

    let out = oak(dir, &["conflict", "take", "tracked.txt", "--ours"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("unbalanced delimiters"),
        "balanced take should not warn, got stderr:\n{}",
        stderr(&out)
    );
}

#[test]
fn conflict_status_json_inspects_checkout_sync_state() {
    let temp = fixture_repo();
    let dir = temp.path();
    write_sync_conflict_fixture(dir);

    let json = json_stdout(&oak(dir, &["conflict", "status", "--json"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["context"], "checkout");
    assert_eq!(json["in_progress"], true);
    assert_eq!(json["kind"], "sync");
    assert_eq!(json["conflict_paths"][0], "tracked.txt");
    assert_eq!(json["recommended_next_commands"][0], "oak pull --continue");
    assert_eq!(json["state"]["sync_head"]["parent_branch"], "main");
    assert_eq!(json["state"]["sync_head"]["branch"], "tester-branch");
    assert_eq!(
        json["state"]["sync_state"]["merged_manifest_hash"],
        "merged123"
    );
}

#[test]
fn conflict_show_json_reports_marker_state() {
    let temp = fixture_repo();
    let dir = temp.path();
    write_sync_conflict_fixture(dir);

    let json = json_stdout(&oak(dir, &["conflict", "show", "--json"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["context"], "checkout");
    assert_eq!(json["conflicts"][0]["path"], "tracked.txt");
    assert_eq!(json["conflicts"][0]["recorded"], true);
    assert_eq!(json["conflicts"][0]["exists"], true);
    assert_eq!(json["conflicts"][0]["has_conflict_markers"], true);
    assert_eq!(json["conflicts"][0]["resolution_state"], "unresolved");
    assert_eq!(json["conflicts"][0]["can_take"], true);
}

#[test]
fn conflict_take_ours_rewrites_checkout_marker_file() {
    let temp = fixture_repo();
    let dir = temp.path();
    write_sync_conflict_fixture(dir);

    let out = oak(dir, &["conflict", "take", "tracked.txt", "--ours"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "before\nours\nafter\n"
    );

    let json = json_stdout(&oak(dir, &["conflict", "show", "--json"]));
    assert_eq!(json["conflicts"][0]["has_conflict_markers"], false);
    assert_eq!(
        json["conflicts"][0]["resolution_state"],
        "resolved_or_binary"
    );
    assert_eq!(json["conflicts"][0]["can_take"], false);
}

#[test]
fn conflict_take_ours_uses_recorded_blob_when_content_contains_separator_line() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-marker"])
        .status
        .success());
    let ours = "Title\n=======\nours body\n";
    std::fs::write(dir.join("tracked.txt"), ours).unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "theirs\n");
    std::fs::write(dir.join(".oak/MERGE_HEAD"), "main\nfeature-marker").unwrap();
    std::fs::write(
        dir.join("tracked.txt"),
        "<<<<<<< mrmrs-f7276c\nTitle\n=======\nours body\n=======\ntheirs\n>>>>>>> main\n",
    )
    .unwrap();

    let out = oak(dir, &["conflict", "take", "tracked.txt", "--ours"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("Re-derived 'tracked.txt' from recorded conflict state"),
        "expected recorded-state fallback warning, got stderr:\n{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        ours
    );
}

#[test]
fn conflict_take_ours_uses_recorded_blob_when_content_contains_theirs_marker_line() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-theirs-marker"])
        .status
        .success());
    let ours = "ours\n";
    std::fs::write(dir.join("tracked.txt"), ours).unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, ">>>>>>> weird content\nB\n");
    std::fs::write(dir.join(".oak/MERGE_HEAD"), "main\nfeature-theirs-marker").unwrap();
    std::fs::write(
        dir.join("tracked.txt"),
        "<<<<<<< mrmrs-f7276c\nours\n=======\n>>>>>>> weird content\nB\n>>>>>>> main\n",
    )
    .unwrap();

    let out = oak(dir, &["conflict", "take", "tracked.txt", "--ours"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("Re-derived 'tracked.txt' from recorded conflict state"),
        "expected recorded-state fallback warning, got stderr:\n{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        ours
    );
}

#[test]
fn conflict_take_ours_preserves_cleanly_merged_theirs_hunks() {
    let temp = fixture_repo();
    let dir = temp.path();
    setup_hunk_conflict(dir);

    let out = oak(dir, &["conflict", "take", "tracked.txt", "--ours"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "A ours\nB theirs\nC ours\n"
    );
}

#[test]
fn conflict_take_ours_json_reports_remaining_state() {
    let temp = fixture_repo();
    let dir = temp.path();
    setup_hunk_conflict(dir);

    let out = oak(
        dir,
        &["conflict", "take", "tracked.txt", "--ours", "--json"],
    );

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["context"], "checkout");
    assert_eq!(json["path"], "tracked.txt");
    assert_eq!(json["side"], "ours");
    assert_eq!(json["remaining_conflict_count"], 0);
    assert_eq!(
        json["remaining_conflict_paths"].as_array().unwrap().len(),
        0
    );
    assert_eq!(json["recommended_next_commands"][0], "oak merge --continue");
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "A ours\nB theirs\nC ours\n"
    );
}

#[test]
fn conflict_take_json_error_uses_structured_envelope() {
    let temp = fixture_repo();
    let dir = temp.path();

    let out = oak(
        dir,
        &["conflict", "take", "tracked.txt", "--ours", "--json"],
    );

    assert!(!out.status.success());
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["error"]["code"], "error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no checkout conflict is in progress"),
        "{json}"
    );
}

#[test]
fn conflict_take_theirs_preserves_cleanly_merged_ours_hunks() {
    let temp = fixture_repo();
    let dir = temp.path();
    setup_hunk_conflict(dir);

    let out = oak(dir, &["conflict", "take", "tracked.txt", "--theirs"]);

    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "A ours\nB theirs\nC theirs\n"
    );
}

fn setup_hunk_conflict(dir: &Path) {
    std::fs::write(dir.join("tracked.txt"), "A base\nB base\nC base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let base = seed_local_main_from_current_head(dir);

    assert!(oak(dir, &["switch", "-c", "feature-hunks"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "A ours\nB base\nC ours\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    advance_main_with_tracked_txt(dir, base, "A base\nB theirs\nC theirs\n");
    std::fs::write(dir.join(".oak/MERGE_HEAD"), "main\nfeature-hunks").unwrap();
    std::fs::write(
        dir.join("tracked.txt"),
        "A ours\nB theirs\n<<<<<<< feature-hunks\nC ours\n=======\nC theirs\n>>>>>>> main\n",
    )
    .unwrap();
}

#[test]
fn agent_state_json_contains_regular_preflight_state() {
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["context"], "checkout");
    assert!(json["branch"].as_str().unwrap().starts_with("tester-"));
    assert_eq!(json["dirty"], true);
    assert_eq!(json["changes"][0]["path"], "tracked.txt");
    assert_eq!(json["recommended_next_commands"][0], "oak commit");
    assert_eq!(json["local_parent_head"], Value::Null);
    assert_eq!(json["remote_parent_head"], Value::Null);
    assert_eq!(json["remote_parent_fetched_at"], Value::Null);
    assert_eq!(json["current_branch_pushed_head"], Value::Null);
    assert_eq!(json["current_branch_push_checked"], false);
    assert_eq!(json["refresh_requested"], false);
    assert_eq!(json["refresh_supported"], true);
    assert_eq!(json["refresh_errors"].as_array().unwrap().len(), 0);
    assert_eq!(json["needs_pull"], false);
    assert_eq!(json["needs_push"], true);
    // This fixture is an UNLINKED checkout (no RepoOwner/RepoName/RemoteUrl), so
    // `oak finish` would push to a remote that does not exist and fail after the
    // commit already mutated the branch. Finish must be blocked honestly.
    assert_eq!(json["can_finish"], false);
    assert_eq!(json["blocking_reason"], "no_remote_configured");
    assert_eq!(json["finish_eligible"], false);
    assert_eq!(json["mount"], Value::Null);
}

#[test]
fn agent_state_json_does_not_store_dirty_file_blob() {
    let temp = fixture_repo();
    let dir = temp.path();

    let dirty_content = "dirty content unique to agent state no-store regression\n";
    let dirty_hash = oak_core::hash_bytes(dirty_content.as_bytes());
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "test setup should start without the dirty blob"
    );

    std::fs::write(dir.join("tracked.txt"), dirty_content).unwrap();
    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["dirty"], true);
    assert_eq!(json["changes"][0]["path"], "tracked.txt");
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "oak agent state --json must not persist dirty working-tree content"
    );
}

#[test]
fn agent_state_json_compact_omits_null_defaults_and_duplicate_finish_alias() {
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

    let full_out = oak(dir, &["agent", "state", "--json"]);
    assert!(full_out.status.success(), "stderr: {}", stderr(&full_out));
    let compact_out = oak(dir, &["agent", "state", "--json", "--compact"]);
    assert!(
        compact_out.status.success(),
        "stderr: {}",
        stderr(&compact_out)
    );
    let compact: Value =
        serde_json::from_str(&stdout(&compact_out)).expect("compact stdout should be JSON");

    assert!(
        stdout(&compact_out).len() < stdout(&full_out).len(),
        "compact output should be smaller\nfull: {}\ncompact: {}",
        stdout(&full_out),
        stdout(&compact_out)
    );
    assert_eq!(compact["schema_version"], 1);
    assert_eq!(compact["context"], "checkout");
    assert_eq!(compact["dirty"], true);
    assert_eq!(compact["change_count"], 1);
    assert_eq!(compact["change_counts"]["modified"], 1);
    assert!(compact.get("changes_omitted").is_none());
    assert_eq!(compact["changes_truncated"], false);
    assert_eq!(compact["changes"][0]["path"], "tracked.txt");
    assert_eq!(compact["finish_eligible"], false);
    assert_eq!(compact["blocking_reason"], "no_remote_configured");
    assert_eq!(compact["recommended_next_commands"][0], "oak commit");

    assert!(compact.get("can_finish").is_none());
    assert!(compact.get("repo_owner").is_none());
    assert!(compact.get("remote_url").is_none());
    assert!(compact.get("local_parent_head").is_none());
    assert!(compact.get("remote_parent_head").is_none());
    assert!(compact.get("remote_parent_fetched_at").is_none());
    assert!(compact.get("current_branch_pushed_head").is_none());
    assert!(compact.get("current_branch_push_checked").is_none());
    assert!(compact.get("refresh_requested").is_none());
    assert!(compact.get("refresh_supported").is_none());
    assert!(compact.get("refresh_errors").is_none());
    assert!(compact.get("mount").is_none());
}

#[test]
fn agent_state_json_compact_bounds_changes_but_preserves_recall_metadata() {
    let temp = fixture_repo();
    let dir = temp.path();
    for i in 0..25 {
        std::fs::write(dir.join(format!("new-{i:02}.txt")), format!("new {i}\n")).unwrap();
    }

    let full = json_stdout(&oak(dir, &["agent", "state", "--json"]));
    assert_eq!(full["changes"].as_array().unwrap().len(), 25);

    let compact = json_stdout(&oak(dir, &["agent", "state", "--json", "--compact"]));

    assert_eq!(compact["schema_version"], 1);
    assert_eq!(compact["dirty"], true);
    assert_eq!(compact["change_count"], 25);
    assert_eq!(compact["change_counts"]["added"], 25);
    assert_eq!(compact["change_counts"]["modified"], 0);
    assert_eq!(compact["change_counts"]["deleted"], 0);
    assert_eq!(compact["change_counts"]["renamed"], 0);
    assert_eq!(compact["changes"].as_array().unwrap().len(), 20);
    assert_eq!(compact["changes_omitted"], 5);
    assert_eq!(compact["changes_truncated"], true);

    let commands = compact["recommended_next_commands"].as_array().unwrap();
    assert!(commands
        .iter()
        .any(|command| command == "oak agent state --json"));
    assert!(commands
        .iter()
        .any(|command| command == "oak status --porcelain"));
}

#[test]
fn agent_state_json_recommends_finish_first_for_linked_dirty_checkout() {
    let temp = fixture_repo();
    let dir = temp.path();
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "agent-json")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.example")
        .unwrap();
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    let commands = json["recommended_next_commands"].as_array().unwrap();
    assert_eq!(commands[0], "oak finish --desc-file <file> --json");
    assert!(commands.iter().any(|cmd| cmd == "oak commit"));
    assert!(commands.iter().any(|cmd| cmd == "oak push"));
}

#[tokio::test]
async fn agent_state_refresh_uses_remote_branch_head_for_push_state() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);
    let head = seed_local_main_from_current_head(dir).to_string();
    let server = MockServer::start().await;

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "agent-json")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/agent-json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": head,
            "name": "agent-json",
            "is_public": true,
            "owner": "oak"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/agent-json/branches/{branch}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": head
        })))
        .mount(&server)
        .await;

    let json = json_stdout(&oak(dir, &["agent", "state", "--json", "--refresh"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["current_branch_pushed_head"], head);
    assert_eq!(json["current_branch_push_checked"], true);
    assert_eq!(json["refresh_requested"], true);
    assert_eq!(json["refresh_supported"], true);
    assert_eq!(json["refresh_errors"].as_array().unwrap().len(), 0);
    assert_eq!(json["needs_push"], false);
    assert_eq!(json["unpushed_commit_count"], 0);
    assert!(!json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd == "oak push"));
}

#[tokio::test]
async fn agent_state_refresh_degrades_when_remote_errors() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);
    seed_local_main_from_current_head(dir);
    let server = MockServer::start().await;

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "agent-json")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/agent-json"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/agent-json/branches/{branch}")))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let json = json_stdout(&oak(dir, &["agent", "state", "--json", "--refresh"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["context"], "checkout");
    assert_eq!(json["refresh_requested"], true);
    assert_eq!(json["refresh_supported"], true);
    assert_eq!(json["current_branch_push_checked"], false);
    assert_eq!(json["remote_parent_head"], Value::Null);
    let errors = json["refresh_errors"].as_array().unwrap();
    assert!(errors
        .iter()
        .any(|e| e.as_str().unwrap().contains("remote_parent_head")));
    assert!(errors
        .iter()
        .any(|e| e.as_str().unwrap().contains("current_branch_pushed_head")));
}

#[test]
fn agent_state_json_reports_finish_eligibility() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    // Link a remote so this clean checkout is genuinely finish-eligible. Without a
    // remote, `oak finish` would commit and then fail on push, so finish is blocked.
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "agent-json")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.example")
        .unwrap();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["context"], "checkout");
    assert_eq!(json["dirty"], false);
    assert_eq!(json["unpushed_commit_count"], 0);
    assert_eq!(json["blocking_reason"], Value::Null);
    assert_eq!(json["can_finish"], true);
    assert_eq!(json["finish_eligible"], true);
    assert_eq!(
        json["recommended_next_commands"][0],
        "oak finish --desc-file <file> --json"
    );
}

#[test]
fn agent_state_json_blocks_finish_for_unlinked_dirty_checkout() {
    // Regression: an UNLINKED checkout (no RepoOwner/RepoName/RemoteUrl) with work to
    // finish must NOT report finish_eligible. `oak finish` commits and then pushes;
    // with no remote the push fails (exit 6) AFTER the commit mutated the branch, so an
    // agent trusting finish_eligible would get a mutate-then-fail. This mirrors
    // `agent_state_json_recommends_finish_first_for_linked_dirty_checkout` but unlinked.
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["dirty"], true);
    assert_eq!(json["can_finish"], false);
    assert_eq!(json["finish_eligible"], false);
    assert_eq!(json["blocking_reason"], "no_remote_configured");
    // No remote → recommend committing, never `oak finish`.
    let commands = json["recommended_next_commands"].as_array().unwrap();
    assert_eq!(commands[0], "oak commit");
    assert!(!commands
        .iter()
        .any(|cmd| cmd.as_str().unwrap().starts_with("oak finish")));
}

#[test]
fn finish_desc_file_json_finishes_clean_unpushed_zero_branch() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "Ship final task\n\nDetails").unwrap();

    let json = json_stdout(&oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    ));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["context"], "checkout");
    assert_eq!(json["branch_description"], "Ship final task\n\nDetails");
    assert_eq!(json["committed"], false);
    assert_eq!(json["pushed"], false);
    assert_eq!(json["description_synced"], false);
    assert_eq!(json["unpushed_before"], 0);
    assert_eq!(json["unpushed_after"], 0);
}

#[test]
fn finish_rejects_empty_description_before_push() {
    let temp = fixture_repo();

    let out = oak(temp.path(), &["finish", "--desc", "   "]);

    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("oak finish requires a non-empty"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn agent_state_json_error_uses_structured_envelope() {
    let temp = tempfile::TempDir::new().unwrap();

    let out = oak(temp.path(), &["agent", "state", "--json"]);

    assert!(!out.status.success());
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["error"]["code"], "repo_not_found");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Repository not found"));
    assert_eq!(json["error"]["conflict_paths"].as_array().unwrap().len(), 0);
    assert_eq!(json["error"]["recommended_next_commands"][0], "oak init");
}

#[test]
fn log_json_is_an_array_of_commit_objects() {
    let temp = fixture_repo();

    let json = json_stdout(&oak(temp.path(), &["log", "--json"]));
    let commits = json.as_array().expect("log JSON should be an array");
    assert_eq!(commits.len(), 1);
    let commit = &commits[0];
    assert!(commit["hash"].as_str().unwrap().len() >= 40);
    assert!(commit["timestamp"].as_str().unwrap().contains('T'));
    assert!(commit["branch"].as_str().unwrap().starts_with("tester-"));
    assert_eq!(commit["description_or_subject"], "1 file");
    assert_eq!(commit["files_changed"], 1);
}

#[test]
fn log_json_path_filter_respects_explicit_limit() {
    let temp = fixture_repo();
    let dir = temp.path();
    for i in 0..5 {
        std::fs::write(dir.join("tracked.txt"), format!("tracked {i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
        std::fs::write(dir.join(format!("other-{i}.txt")), format!("other {i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
    }

    let json = json_stdout(&oak(dir, &["log", "--json", "-n", "2", "tracked.txt"]));
    let commits = json.as_array().expect("log JSON should be an array");

    assert_eq!(commits.len(), 2);
    assert!(commits
        .iter()
        .all(|commit| commit["files_changed"].as_u64() == Some(1)));
}

#[test]
fn branch_list_json_defaults_from_bare_branch_command() {
    let temp = fixture_repo();

    let json = json_stdout(&oak(temp.path(), &["branch", "list", "--json"]));
    let branches = json
        .as_array()
        .expect("branch list JSON should be an array");
    assert_eq!(branches.len(), 1);
    let branch = &branches[0];
    assert_eq!(branch["schema_version"], 1);
    assert!(branch["name"].as_str().unwrap().starts_with("tester-"));
    assert!(branch["head"].as_str().unwrap().len() >= 40);
    assert_eq!(branch["description"], Value::Null);
    assert_eq!(branch["status"], "open");
    assert_eq!(branch["current"], true);
    assert!(branch["created_at"].as_str().unwrap().contains('T'));

    let bare = json_stdout(&oak(temp.path(), &["branch", "--json"]));
    assert_eq!(bare, json);

    let show = json_stdout(&oak(
        temp.path(),
        &["branch", "show", branch["name"].as_str().unwrap(), "--json"],
    ));
    assert_eq!(show["schema_version"], 1);
    assert_eq!(show["name"], branch["name"]);
    assert_eq!(show["head"], branch["head"]);
    assert_eq!(show["current"], true);
}

#[test]
fn branch_show_json_matches_list_row_without_requiring_current_branch() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(oak(dir, &["switch", "-c", "feature-show-json"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "feature\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["switch", "-c", "other-current"])
        .status
        .success());

    let list = json_stdout(&oak(dir, &["branch", "list", "--json"]));
    let branches = list.as_array().unwrap();
    let listed = branches
        .iter()
        .find(|branch| branch["name"] == "feature-show-json")
        .expect("feature branch should be listed");

    let show = json_stdout(&oak(
        dir,
        &["branch", "show", "feature-show-json", "--json"],
    ));

    assert_eq!(&show, listed);
    assert_eq!(show["current"], false);
    assert!(show["head"].as_str().unwrap().len() >= 40);
}

#[test]
fn branch_show_json_reports_missing_branch() {
    let temp = fixture_repo();

    let out = oak(temp.path(), &["branch", "show", "missing", "--json"]);

    assert!(!out.status.success());
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["error"]["code"], "error");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Branch not found"));
}

#[tokio::test(flavor = "current_thread")]
async fn remote_branch_list_show_diff_review_do_not_switch_checkout() {
    let temp = fixture_repo();
    let dir = temp.path();
    let main_head = seed_local_main_from_current_head(dir);
    let feature_head = seed_remote_only_feature_commit(dir, main_head.clone());
    let original_branch = current_branch(dir);

    let server = MockServer::start().await;
    mount_remote_branch_list(&server, &main_head, &feature_head).await;
    link_remote(dir, &server);

    let list = json_stdout(&oak(
        dir,
        &["branch", "list", "--remote", "--status", "open", "--json"],
    ));
    let branches = list.as_array().unwrap();
    assert_eq!(branches.len(), 2, "closed branch should be filtered out");
    assert!(branches
        .iter()
        .any(|branch| branch["name"] == "remote-feature" && branch["remote"] == true));
    assert_eq!(current_branch(dir), original_branch);

    let show = json_stdout(&oak(
        dir,
        &["branch", "show", "remote-feature", "--remote", "--json"],
    ));
    assert_eq!(show["schema_version"], 1);
    assert_eq!(show["name"], "remote-feature");
    assert_eq!(show["head"], feature_head.as_str());
    assert_eq!(show["current"], false);
    assert_eq!(current_branch(dir), original_branch);

    let diff = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "remote-feature",
            "--remote",
            "--against",
            "main",
            "--json",
        ],
    ));
    assert_eq!(diff["schema_version"], 1);
    assert_eq!(diff["kind"], "remote_branch_diff");
    assert_eq!(diff["branch"], "remote-feature");
    assert_eq!(diff["changed_file_count"], 1);
    assert_eq!(diff["changed_files"][0]["path"], "tracked.txt");
    assert_eq!(current_branch(dir), original_branch);

    let review = json_stdout(&oak(
        dir,
        &[
            "branch",
            "review",
            "remote-feature",
            "--remote",
            "--merge-preview",
            "--json",
        ],
    ));
    assert_eq!(review["schema_version"], 1);
    assert_eq!(review["branch"], "remote-feature");
    assert!(review["merge_preview"]["prediction_available"]
        .as_bool()
        .unwrap());
    assert_eq!(
        review["recommended_next_commands"],
        serde_json::json!(["oak switch remote-feature", "oak merge"])
    );
    assert_eq!(current_branch(dir), original_branch);
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "base\n",
        "remote review must not rewrite the worktree"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn close_remote_branch_json_does_not_switch_checkout() {
    let temp = fixture_repo();
    let dir = temp.path();
    let main_head = seed_local_main_from_current_head(dir);
    let feature_head = seed_remote_only_feature_commit(dir, main_head.clone());
    let original_branch = current_branch(dir);

    let server = MockServer::start().await;
    mount_remote_branch_list(&server, &main_head, &feature_head).await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;
    link_remote(dir, &server);

    let closed = json_stdout(&oak(
        dir,
        &[
            "close",
            "remote-feature",
            "--remote",
            "--reason",
            "superseded",
            "--json",
        ],
    ));

    assert_eq!(closed["schema_version"], 1);
    assert_eq!(closed["branch"], "remote-feature");
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["close_reason"], "superseded");
    assert_eq!(closed["remote"], true);
    assert_eq!(current_branch(dir), original_branch);

    let show = json_stdout(&oak(dir, &["branch", "show", "remote-feature", "--json"]));
    assert_eq!(show["status"], "closed");
    assert_eq!(show["close_reason"], "superseded");
}

#[test]
fn close_local_branch_json_persists_reason_in_list_and_show() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = current_branch(dir);
    assert!(oak(dir, &["switch", "-c", "feature-close-reason"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "feature work\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["switch", &original_branch, "--clean"])
        .status
        .success());

    let closed = json_stdout(&oak(
        dir,
        &[
            "close",
            "feature-close-reason",
            "--reason",
            "stale",
            "--json",
        ],
    ));
    assert_eq!(closed["schema_version"], 1);
    assert_eq!(closed["branch"], "feature-close-reason");
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["close_reason"], "stale");
    assert_eq!(closed["remote"], false);

    let listed = json_stdout(&oak(
        dir,
        &["branch", "list", "--status", "closed", "--json"],
    ))
    .as_array()
    .unwrap()
    .iter()
    .find(|branch| branch["name"] == "feature-close-reason")
    .expect("closed branch should appear in filtered list")
    .clone();
    assert_eq!(listed["status"], "closed");
    assert_eq!(listed["close_reason"], "stale");

    let show = json_stdout(&oak(
        dir,
        &["branch", "show", "feature-close-reason", "--json"],
    ));
    assert_eq!(show["status"], "closed");
    assert_eq!(show["close_reason"], "stale");
}

#[test]
fn close_without_reason_still_works_and_omits_close_reason() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = current_branch(dir);
    assert!(oak(dir, &["switch", "-c", "feature-no-reason"])
        .status
        .success());
    assert!(oak(dir, &["switch", &original_branch, "--clean"])
        .status
        .success());

    assert!(oak(dir, &["close", "feature-no-reason"]).status.success());

    let show = json_stdout(&oak(
        dir,
        &["branch", "show", "feature-no-reason", "--json"],
    ));
    assert_eq!(show["status"], "closed");
    assert!(show.get("close_reason").is_none());
}

#[test]
fn close_invalid_reason_exits_usage_error() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = current_branch(dir);
    assert!(oak(dir, &["switch", "-c", "feature-bad-reason"])
        .status
        .success());
    assert!(oak(dir, &["switch", &original_branch, "--clean"])
        .status
        .success());

    let out = oak(
        dir,
        &[
            "close",
            "feature-bad-reason",
            "--reason",
            "merged",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn diff_json_summarizes_working_tree_changes() {
    let temp = fixture_repo();
    let dir = temp.path();
    let modified_content = "changed by diff json no-store regression\n";
    let added_content = "new by diff json no-store regression\n";
    let modified_hash = oak_core::hash_bytes(modified_content.as_bytes());
    let added_hash = oak_core::hash_bytes(added_content.as_bytes());
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert!(!repo.has_blob(&modified_hash).unwrap());
    assert!(!repo.has_blob(&added_hash).unwrap());

    std::fs::write(dir.join("tracked.txt"), modified_content).unwrap();
    std::fs::write(dir.join("new.txt"), added_content).unwrap();

    let json = json_stdout(&oak(dir, &["diff", "--json"]));

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["kind"], "working_tree_diff");
    assert_eq!(json["changed_file_count"], 2);
    assert!(
        json.get("changed_files_page").is_none(),
        "default diff JSON should remain exhaustive"
    );
    let files = json["changed_files"].as_array().unwrap();
    assert!(files.iter().any(|f| {
        f["path"] == "tracked.txt"
            && f["status"] == "modified"
            && f["additions"].is_number()
            && f["deletions"].is_number()
            && f.get("stats_available").is_none()
    }));
    assert!(files
        .iter()
        .any(|f| f["path"] == "new.txt" && f["status"] == "added"));
    assert_eq!(json["recommended_next_commands"][0], "oak diff --print");
    assert!(
        !repo.has_blob(&modified_hash).unwrap(),
        "oak diff --json must not persist modified working-tree content"
    );
    assert!(
        !repo.has_blob(&added_hash).unwrap(),
        "oak diff --json must not persist added working-tree content"
    );
}

#[test]
fn diff_json_changed_files_can_be_paged_without_losing_total_recall() {
    let temp = fixture_repo();
    let dir = temp.path();
    for i in 0..5 {
        std::fs::write(dir.join(format!("new-{i:02}.txt")), format!("new {i}\n")).unwrap();
    }

    let first = json_stdout(&oak(dir, &["diff", "--json", "--changed-files-limit", "2"]));

    assert_eq!(first["changed_file_count"], 5);
    assert_eq!(first["changed_files"].as_array().unwrap().len(), 2);
    assert_eq!(first["changed_files_page"]["offset"], 0);
    assert_eq!(first["changed_files_page"]["limit"], 2);
    assert_eq!(first["changed_files_page"]["total_count"], 5);
    assert_eq!(first["changed_files_page"]["returned_count"], 2);
    assert_eq!(first["changed_files_page"]["omitted_count"], 3);
    assert!(first["changed_files_page"].get("total").is_none());
    assert_eq!(first["changed_files_page"]["next_offset"], 2);
    assert!(first["changed_files_page"]["next_page_command"]
        .as_str()
        .unwrap()
        .contains("--changed-files-offset 2"));
    assert_eq!(first["recommended_next_commands"][0], "oak diff --json");

    let second = json_stdout(&oak(
        dir,
        &[
            "diff",
            "--json",
            "--changed-files-limit",
            "2",
            "--changed-files-offset",
            "2",
        ],
    ));

    assert_eq!(second["changed_file_count"], 5);
    assert_eq!(second["changed_files"].as_array().unwrap().len(), 2);
    assert_eq!(second["changed_files_page"]["offset"], 2);
    assert_eq!(second["changed_files_page"]["next_offset"], 4);
    assert_eq!(second["changed_files"][0]["path"], "new-02.txt");
}

#[test]
fn diff_changed_files_limit_requires_json() {
    let temp = fixture_repo();

    let out = oak(temp.path(), &["diff", "--changed-files-limit", "2"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("required"),
        "stderr should explain the missing --json requirement:\n{}",
        stderr(&out)
    );
}

#[test]
fn branch_diff_and_review_json_include_changed_files_and_lineage() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-json"]).status.success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let diff = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-json",
            "--against",
            "main",
            "--json",
        ],
    ));

    assert_eq!(diff["schema_version"], 1);
    assert_eq!(diff["branch"], "feature-json");
    assert_eq!(diff["against"], "main");
    assert_eq!(diff["changed_file_count"], 1);
    assert_eq!(diff["changed_files"][0]["path"], "tracked.txt");
    assert_eq!(diff["changed_files"][0]["status"], "modified");
    assert_eq!(diff["merge_lineage_evidence"]["branch_parent"], "main");
    assert!(diff["branch_head"].as_str().unwrap().len() >= 40);

    let review = json_stdout(&oak(dir, &["branch", "review", "feature-json", "--json"]));
    assert_eq!(review["schema_version"], 1);
    assert_eq!(review["changed_file_count"], 1);
    assert!(review["merge_preview"].is_null());
    assert!(review["files_identical_to_against"]["available"]
        .as_bool()
        .unwrap());
    assert_eq!(
        review["recommended_next_commands"],
        serde_json::json!(["oak branch review feature-json --merge-preview --json"])
    );
}

#[test]
fn branch_diff_json_limit_reports_omitted_files_without_changing_total() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-json-limit"])
        .status
        .success());
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let full = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-json-limit",
            "--against",
            "main",
            "--json",
        ],
    ));
    assert_eq!(full["changed_file_count"], 3);
    assert_eq!(full["changed_files"].as_array().unwrap().len(), 3);
    assert!(full.get("changed_files_page").is_none());

    let limited = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-json-limit",
            "--against",
            "main",
            "--changed-files-limit",
            "1",
            "--json",
        ],
    ));
    assert_eq!(limited["changed_file_count"], 3);
    assert_eq!(limited["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(limited["changed_files_page"]["total_count"], 3);
    assert_eq!(limited["changed_files_page"]["returned_count"], 1);
    assert_eq!(limited["changed_files_page"]["omitted_count"], 2);
    assert_eq!(limited["changed_files_page"]["next_offset"], 1);
    assert!(limited["changed_files_page"]["next_page_command"]
        .as_str()
        .unwrap()
        .contains("oak branch diff feature-json-limit"));
    assert!(limited["changed_files_page"]["next_page_command"]
        .as_str()
        .unwrap()
        .contains("--changed-files-offset 1"));
}

#[test]
fn branch_review_merge_preview_json_predicts_local_conflicts() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-conflict"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    std::fs::write(dir.join("branch-only.txt"), "branch-only\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");

    let review = json_stdout(&oak(
        dir,
        &[
            "branch",
            "review",
            "feature-conflict",
            "--merge-preview",
            "--json",
        ],
    ));

    let preview = &review["merge_preview"];
    assert_eq!(preview["schema_version"], 1);
    assert_eq!(preview["prediction_available"], true);
    assert_eq!(preview["clean"], false);
    assert_eq!(preview["conflict_file_count"], 1);
    assert_eq!(preview["conflict_files"][0]["path"], "tracked.txt");
    assert_eq!(preview["conflict_files"][0]["conflict_type"], "content");
    assert!(
        preview["changed_files"].is_array(),
        "embedded preview changed_files must be kept when it differs from the review summary"
    );

    let dry_run = json_stdout(&oak(dir, &["merge", "--dry-run", "--json"]));
    assert_eq!(dry_run["schema_version"], 1);
    assert_eq!(dry_run["branch"], current_branch(dir));
    assert_eq!(dry_run["conflict_file_count"], 1);
    assert_eq!(preview["changed_file_count"], dry_run["changed_file_count"]);
    assert_eq!(preview["changed_files"], dry_run["changed_files"]);
    assert_ne!(
        review["changed_files"], preview["changed_files"],
        "review and embedded preview represent different diffs in this conflict case"
    );
    assert_eq!(
        review["recommended_next_commands"],
        serde_json::json!(["oak switch feature-conflict", "oak pull", "oak merge"])
    );
}

#[test]
fn branch_review_merge_preview_json_limit_preserves_distinct_preview_metadata() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-preview-limit"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    std::fs::write(dir.join("branch-only-a.txt"), "a\n").unwrap();
    std::fs::write(dir.join("branch-only-b.txt"), "b\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");

    let review = json_stdout(&oak(
        dir,
        &[
            "branch",
            "review",
            "feature-preview-limit",
            "--merge-preview",
            "--changed-files-limit",
            "1",
            "--json",
        ],
    ));

    assert_eq!(review["changed_file_count"], 3);
    assert_eq!(review["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(review["changed_files_page"]["omitted_count"], 2);

    let preview = &review["merge_preview"];
    assert_eq!(preview["prediction_available"], true);
    assert_eq!(preview["clean"], false);
    assert_eq!(preview["changed_file_count"], 3);
    assert_eq!(preview["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(preview["changed_files_page"]["omitted_count"], 2);
    assert_eq!(preview["conflict_files"][0]["path"], "tracked.txt");
}

#[test]
fn branch_diff_diff_mode_tree_includes_target_drift() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-diff-modes"])
        .status
        .success());
    std::fs::write(dir.join("branch-only.txt"), "branch-only\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");

    let tree = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-diff-modes",
            "--against",
            "main",
            "--diff-mode",
            "tree",
            "--json",
        ],
    ));
    assert_eq!(tree["diff_mode"], "tree");
    assert_eq!(tree["changed_file_count"], 2);
    let paths: Vec<_> = tree["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"tracked.txt"));
    assert!(paths.contains(&"branch-only.txt"));
    assert_eq!(
        tree["merge_lineage_evidence"]["comparison_source"],
        "local_branch_head:main"
    );
}

#[test]
fn branch_diff_diff_mode_contribution_excludes_target_drift() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-diff-modes"])
        .status
        .success());
    std::fs::write(dir.join("branch-only.txt"), "branch-only\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");

    let contribution = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-diff-modes",
            "--against",
            "main",
            "--diff-mode",
            "contribution",
            "--json",
        ],
    ));
    assert_eq!(contribution["diff_mode"], "contribution");
    assert_eq!(contribution["changed_file_count"], 1);
    assert_eq!(contribution["changed_files"][0]["path"], "branch-only.txt");
    assert_eq!(
        contribution["merge_lineage_evidence"]["comparison_source"],
        "fork_point"
    );

    let tree = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-diff-modes",
            "--against",
            "main",
            "--diff-mode",
            "tree",
            "--json",
        ],
    ));
    assert_eq!(tree["changed_file_count"], 2);
    assert!(
        tree["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "tracked.txt"),
        "tree mode should include target drift on tracked.txt"
    );
}

#[test]
fn branch_diff_diff_mode_net_merge_shows_predicted_post_merge_effect() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-diff-modes"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    std::fs::write(dir.join("branch-only.txt"), "branch-only\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");

    let net_merge = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-diff-modes",
            "--against",
            "main",
            "--diff-mode",
            "net-merge",
            "--json",
        ],
    ));
    assert_eq!(net_merge["diff_mode"], "net-merge");
    assert_eq!(
        net_merge["merge_lineage_evidence"]["comparison_source"],
        "predicted_merge_result"
    );
    assert!(
        net_merge["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "branch-only.txt"),
        "net-merge should include branch-only additions: {:?}",
        net_merge["changed_files"]
    );
    assert!(
        net_merge["caveats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|caveat| caveat.as_str().unwrap().contains("conflict")),
        "net-merge should report that tracked.txt conflicted: {:?}",
        net_merge["caveats"]
    );

    let tree = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-diff-modes",
            "--against",
            "main",
            "--diff-mode",
            "tree",
            "--json",
        ],
    ));
    let tree_tracked = tree["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "tracked.txt")
        .expect("tree diff should include tracked.txt");
    let net_tracked = net_merge["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "tracked.txt")
        .expect("net-merge diff should include tracked.txt fallout");
    assert_eq!(tree_tracked["status"], "modified");
    assert_eq!(net_tracked["status"], "deleted");
}

#[test]
fn merge_dry_run_json_accepts_explicit_branch_without_switching_checkout() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-dry-run"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    std::fs::write(dir.join("branch-only.txt"), "branch-only\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");
    assert!(oak(dir, &["switch", "-c", "observer"]).status.success());
    assert_eq!(current_branch(dir), "observer");

    let dry_run = json_stdout(&oak(
        dir,
        &["merge", "--dry-run", "feature-dry-run", "--json"],
    ));
    assert_eq!(dry_run["schema_version"], 1);
    assert_eq!(dry_run["branch"], "feature-dry-run");
    assert_eq!(current_branch(dir), "observer");
    assert_eq!(dry_run["prediction_available"], true);
    assert_eq!(dry_run["clean"], false);
    assert_eq!(dry_run["conflict_file_count"], 1);
    assert_eq!(dry_run["conflict_files"][0]["path"], "tracked.txt");
}

#[test]
fn reset_refuses_non_interactive_discard_with_dirty_exit_code() {
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

    let out = oak(dir, &["reset"]);

    assert_eq!(out.status.code(), Some(4));
    assert!(
        stderr(&out).contains(
            "refusing to discard 1 change(s) without --force when not running interactively"
        ),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "changed\n"
    );
}

#[test]
fn held_workdir_lock_exits_with_retryable_lock_code() {
    let temp = fixture_repo();
    std::fs::write(
        temp.path().join(".oak/wdlock"),
        std::process::id().to_string(),
    )
    .unwrap();

    let out = oak(temp.path(), &["commit"]);

    assert_eq!(out.status.code(), Some(3));
    assert!(
        stderr(&out).contains("Repository is locked by another process"),
        "stderr: {}",
        stderr(&out)
    );
}

/// A filename the tree format cannot represent (`\n` is a tree-encoding
/// delimiter) is a usage error, not a generic failure: exit code 2 so an
/// unattended agent can tell "fix your input" from "something broke".
#[test]
fn hostile_filename_commit_exits_with_usage_code() {
    let temp = fixture_repo();
    std::fs::write(temp.path().join("a\nb.txt"), "x\n").unwrap();

    let out = oak(temp.path(), &["commit"]);
    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(2),
        "InvalidPath should map to the usage exit code\nstderr:\n{}",
        stderr(&out)
    );
}

#[test]
fn branch_review_triage_empty_branch_recommends_close() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "empty-branch"]).status.success());

    let review = json_stdout(&oak(dir, &["branch", "review", "empty-branch", "--json"]));

    assert_eq!(review["recommended_action"], "close");
    assert_eq!(review["reason"], "empty");
    assert_eq!(review["close_allowed"], true);
    assert_eq!(review["contribution"], "empty");
    assert_eq!(review["merge_allowed"], false);
    assert_eq!(review["checks"]["required"], true);
    assert_eq!(review["checks"]["known_passed"], false);
    assert_eq!(review["checks"]["source"], Value::Null);
    assert_eq!(review["vcs_merge_safe"], Value::Null);
}

#[test]
fn branch_review_triage_superseded_exact_when_target_matches_branch() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-superseded"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "already on main\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "already on main\n");

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-superseded", "--json"],
    ));

    assert_eq!(review["recommended_action"], "close");
    assert_eq!(review["reason"], "superseded_exact");
    assert_eq!(review["close_allowed"], true);
    assert_eq!(review["contribution"], "superseded_exact");
    assert_eq!(review["changed_file_count"], 0);
    assert_eq!(review["merge_allowed"], false);
}

#[test]
fn branch_review_triage_clean_contributor_recommends_validate_then_merge() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-clean"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch contribution\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let review = json_stdout(&oak(dir, &["branch", "review", "feature-clean", "--json"]));

    assert_eq!(review["recommended_action"], "validate_then_merge");
    assert_eq!(review["reason"], "clean_contribution");
    assert_eq!(review["close_allowed"], false);
    assert_eq!(review["contribution"], "contributes");
    assert_eq!(review["mergeability"], "clean");
    assert_eq!(review["vcs_merge_safe"], true);
    assert_eq!(review["merge_allowed"], false);
    assert_eq!(review["target_risk"], "none");
}

#[test]
fn branch_review_triage_conflict_stays_review_when_target_risk_unknown() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-conflict-triage"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-conflict-triage", "--json"],
    ));

    assert_eq!(review["recommended_action"], "review");
    assert_eq!(review["reason"], "target_risk_unknown");
    assert_eq!(review["close_allowed"], false);
    assert_eq!(review["mergeability"], "conflicts");
    assert_eq!(review["target_risk"], "unknown");
    assert_eq!(review["vcs_merge_safe"], false);
    assert_eq!(review["merge_allowed"], false);
}

#[test]
fn branch_triage_conflict_stays_review_when_target_risk_unknown() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-conflict-batch"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--json",
        ],
    ));
    let row = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == "feature-conflict-batch")
        .expect("feature-conflict-batch row");

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-conflict-batch", "--json"],
    ));

    assert_eq!(row["recommended_action"], "review");
    assert_eq!(row["reason"], "target_risk_unknown");
    assert_eq!(row["mergeability"], "conflicts");
    assert_eq!(row["target_risk"], "unknown");
    assert_eq!(row["vcs_merge_safe"], false);
    assert_eq!(row["recommended_action"], review["recommended_action"]);
    assert_eq!(row["reason"], review["reason"]);
    assert_eq!(row["target_risk"], review["target_risk"]);
    assert_eq!(row["vcs_merge_safe"], review["vcs_merge_safe"]);
}

#[test]
fn branch_review_triage_superseded_fallback_without_against_head_stays_safe() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(oak(dir, &["switch", "-c", "feature-superseded-fallback"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "side trip\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-superseded-fallback", "--json"],
    ));

    assert_ne!(review["recommended_action"], "close");
    assert_eq!(review["close_allowed"], false);
    assert_eq!(review["merge_allowed"], false);
    assert_eq!(review["contribution"], "unknown");
    assert_eq!(review["changed_file_count"], 0);
    assert!(review["missing_data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item.as_str() == Some("against_head_unavailable")));
}

#[test]
fn branch_review_triage_missing_against_head_stays_safe() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(oak(dir, &["switch", "-c", "feature-no-main"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch only\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-no-main", "--json"],
    ));

    assert_eq!(review["recommended_action"], "review");
    assert_eq!(review["close_allowed"], false);
    assert_eq!(review["merge_allowed"], false);
    assert_eq!(review["vcs_merge_safe"], Value::Null);
    assert!(review["missing_data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item.as_str() == Some("against_head_unavailable")));
}

#[tokio::test(flavor = "current_thread")]
async fn branch_review_triage_remote_does_not_switch_checkout() {
    let temp = fixture_repo();
    let dir = temp.path();
    let main_head = seed_local_main_from_current_head(dir);
    let feature_head = seed_remote_only_feature_commit(dir, main_head.clone());
    let original_branch = current_branch(dir);

    let server = MockServer::start().await;
    mount_remote_branch_list(&server, &main_head, &feature_head).await;
    link_remote(dir, &server);

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "remote-feature", "--remote", "--json"],
    ));

    assert_eq!(review["recommended_action"], "validate_then_merge");
    assert_eq!(review["contribution"], "contributes");
    assert_eq!(review["mergeability"], "clean");
    assert_eq!(review["vcs_merge_safe"], true);
    assert_eq!(review["merge_allowed"], false);
    assert_eq!(current_branch(dir), original_branch);
}

fn seed_local_triage_branches(dir: &Path) -> String {
    let main_head = seed_local_main_from_current_head(dir);
    let original_branch = current_branch(dir);

    assert!(oak(dir, &["switch", "-c", "empty-branch"]).status.success());

    assert!(oak(dir, &["switch", "-c", "superseded-branch"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "same\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["switch", &original_branch]).status.success());
    advance_main_with_tracked_txt(dir, main_head, "same\n");

    assert!(oak(dir, &["switch", "-c", "feature-branch"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "unique\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    assert!(oak(dir, &["switch", "-c", "closed-branch"])
        .status
        .success());
    std::fs::write(dir.join("closed.txt"), "closed\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["close", "closed-branch"]).status.success());

    assert!(oak(dir, &["switch", &original_branch]).status.success());
    original_branch
}

#[test]
fn branch_triage_json_emits_one_row_per_open_branch_without_switching_checkout() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = seed_local_triage_branches(dir);

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--json",
        ],
    ));

    assert_eq!(triage["schema_version"], 1);
    assert_eq!(triage["against"], "main");
    assert_eq!(triage["remote"], false);
    let rows = triage["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "open branches except main");
    assert!(rows.iter().any(|row| row["branch"] == "empty-branch"));
    assert!(rows.iter().any(|row| row["branch"] == "superseded-branch"));
    assert!(rows.iter().any(|row| row["branch"] == "feature-branch"));
    assert!(!rows.iter().any(|row| row["branch"] == "closed-branch"));
    assert!(!rows.iter().any(|row| row["branch"] == "main"));
    assert!(rows
        .iter()
        .all(|row| row["merge_allowed"].as_bool() == Some(false)));
    assert!(rows.iter().all(|row| row.get("mergeability").is_some()));
    assert!(rows.iter().all(|row| row.get("contribution").is_some()));
    assert!(rows.iter().all(|row| row.get("target_risk").is_some()));
    assert!(rows.iter().all(|row| row.get("checks").is_some()));
    assert_eq!(current_branch(dir), original_branch);
}

#[test]
fn branch_triage_json_only_closable_returns_exact_close_eligible_branches() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_triage_branches(dir);

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--only",
            "closable",
            "--json",
        ],
    ));

    let rows = triage["rows"].as_array().unwrap();
    assert!(rows.len() >= 2);
    assert!(rows
        .iter()
        .all(|row| row["close_allowed"].as_bool() == Some(true)));
    let names: Vec<_> = rows
        .iter()
        .map(|row| row["branch"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"empty-branch"));
    assert!(names.contains(&"superseded-branch"));
    assert!(!names.contains(&"feature-branch"));
}

#[test]
fn branch_triage_json_limit_defers_remaining_branches_honestly() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_triage_branches(dir);

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--limit",
            "1",
            "--json",
        ],
    ));

    assert_eq!(triage["branches_analyzed"], 1);
    assert_eq!(triage["branches_deferred"], 3);
    let rows = triage["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "deferred rows must not be silently dropped");
    assert_eq!(
        rows.iter()
            .filter(|row| row["deferred"].as_bool() == Some(true))
            .count(),
        3
    );
}

#[test]
fn branch_triage_json_status_open_filters_closed_branches() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_triage_branches(dir);

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "closed",
            "--json",
        ],
    ));

    let rows = triage["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["branch"], "closed-branch");
}

#[tokio::test(flavor = "current_thread")]
async fn remote_branch_triage_json_does_not_switch_checkout() {
    let temp = fixture_repo();
    let dir = temp.path();
    let main_head = seed_local_main_from_current_head(dir);
    let feature_head = seed_remote_only_feature_commit(dir, main_head.clone());
    let original_branch = current_branch(dir);

    let server = MockServer::start().await;
    mount_remote_branch_list(&server, &main_head, &feature_head).await;
    link_remote(dir, &server);

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--remote",
            "--against",
            "main",
            "--status",
            "open",
            "--json",
        ],
    ));

    assert_eq!(triage["schema_version"], 1);
    assert_eq!(triage["against"], "main");
    assert_eq!(triage["remote"], true);
    let rows = triage["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["branch"], "remote-feature");
    assert!(rows[0]["merge_allowed"].as_bool() == Some(false));
    assert_eq!(current_branch(dir), original_branch);
}

#[test]
fn branch_triage_batch_row_matches_single_branch_review() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_triage_branches(dir);

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--json",
        ],
    ));
    let batch_row = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == "feature-branch")
        .expect("feature-branch row");

    let review = json_stdout(&oak(dir, &["branch", "review", "feature-branch", "--json"]));

    assert_eq!(
        batch_row["recommended_action"],
        review["recommended_action"]
    );
    assert_eq!(batch_row["reason"], review["reason"]);
    assert_eq!(batch_row["close_allowed"], review["close_allowed"]);
    assert_eq!(batch_row["vcs_merge_safe"], review["vcs_merge_safe"]);
    assert_eq!(batch_row["mergeability"], review["mergeability"]);
    assert_eq!(batch_row["contribution"], review["contribution"]);
    assert_eq!(batch_row["target_risk"], review["target_risk"]);
    assert_eq!(batch_row["checks"], review["checks"]);
}

#[test]
fn branch_triage_hunk_inclusion_superseded_recommends_close() {
    let temp = fixture_repo();
    let dir = temp.path();
    let main_head = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "hunk-superseded"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "alpha\nbeta\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, main_head, "prefix\nalpha\nbeta\nsuffix\n");

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--json",
        ],
    ));
    let row = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == "hunk-superseded")
        .expect("hunk-superseded row");

    assert_eq!(row["recommended_action"], "close");
    assert_eq!(row["reason"], "superseded_exact");
    assert_eq!(row["close_allowed"], true);
    assert_eq!(row["contribution"], "superseded_exact");
    assert_eq!(row["target_risk"], "unknown");
    assert_ne!(row["recommended_action"], "validate_then_merge");
}

#[test]
fn branch_triage_missing_against_head_stays_safe() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(oak(dir, &["switch", "-c", "feature-no-main-batch"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch only\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--json",
        ],
    ));
    let row = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == "feature-no-main-batch")
        .expect("feature-no-main-batch row");

    assert_eq!(row["recommended_action"], "review");
    assert_eq!(row["close_allowed"], false);
    assert_eq!(row["vcs_merge_safe"], Value::Null);
    assert!(row["missing_data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item.as_str() == Some("against_head_unavailable")));
}

#[test]
fn branch_triage_superseded_fallback_without_against_head_stays_safe() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(
        oak(dir, &["switch", "-c", "feature-superseded-fallback-batch"])
            .status
            .success()
    );
    std::fs::write(dir.join("tracked.txt"), "side trip\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--json",
        ],
    ));
    let row = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == "feature-superseded-fallback-batch")
        .expect("feature-superseded-fallback-batch row");

    assert_ne!(row["recommended_action"], "close");
    assert_eq!(row["close_allowed"], false);
    assert_eq!(row["merge_allowed"], false);
    assert_eq!(row["contribution"], "unknown");
    assert_eq!(row["unique_contribution"]["changed_file_count"], 0);
    assert!(row["missing_data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item.as_str() == Some("against_head_unavailable")));
}

#[test]
fn branch_review_triage_hunk_inclusion_superseded_recommends_close() {
    let temp = fixture_repo();
    let dir = temp.path();
    let main_head = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "hunk-superseded-review"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "alpha\nbeta\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, main_head, "prefix\nalpha\nbeta\nsuffix\n");

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "hunk-superseded-review", "--json"],
    ));

    assert_eq!(review["recommended_action"], "close");
    assert_eq!(review["reason"], "superseded_exact");
    assert_eq!(review["close_allowed"], true);
    assert_eq!(review["contribution"], "superseded_exact");
    assert_eq!(review["target_risk"], "unknown");
    assert_ne!(review["recommended_action"], "validate_then_merge");
}

#[test]
fn branch_triage_summary_depth_skips_merge_prediction() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_triage_branches(dir);

    let triage = json_stdout(&oak(
        dir,
        &[
            "branch",
            "triage",
            "--against",
            "main",
            "--status",
            "open",
            "--analysis-depth",
            "summary",
            "--json",
        ],
    ));

    assert!(triage["caveats"].as_array().unwrap().iter().any(|item| {
        item.as_str()
            .is_some_and(|text| text.contains("Summary analysis depth skips merge prediction"))
    }));
    let rows = triage["rows"].as_array().unwrap();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| row["vcs_merge_safe"] == Value::Null));
    assert!(rows.iter().all(|row| {
        row["missing_data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str() == Some("merge_prediction_unavailable"))
    }));
}
