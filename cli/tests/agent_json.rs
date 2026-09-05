//! Machine-readable output and non-interactive safety contracts.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{
    Branch, ChangeType, Commit, FileChange, FileMode, Manifest, ManifestEntry, MetadataKey,
    Repository, SqliteRepository,
};
use serde_json::Value;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const AGENT_STATE_SCHEMA_VERSION: i64 = 2;

fn oak(dir: &Path, args: &[&str]) -> Output {
    oak_with_env(dir, args, &[])
}

fn oak_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("oak binary should run")
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

/// Record that local `main` was verified against the remote just now, so
/// merge-safety certification treats local target data as fresh (fb-105).
fn mark_main_checked_now(dir: &Path) {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    repo.set_metadata(MetadataKey::MainLastCheckedAt, &now.to_string())
        .unwrap();
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

fn advance_main_adding_file(dir: &Path, parent: oak_core::Hash, path: &str, content: &str) {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let parent_commit = repo.get_commit(&parent).unwrap().unwrap();
    let parent_manifest = repo
        .get_manifest(&parent_commit.manifest_hash)
        .unwrap()
        .unwrap();
    let new_blob = repo.put_blob(content.as_bytes().to_vec()).unwrap();
    let mut entries = parent_manifest.entries.clone();
    entries.push(ManifestEntry {
        path: path.to_string(),
        blob_hash: new_blob.clone(),
        mode: FileMode::Regular,
    });
    let manifest = Manifest::new(entries);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "main".to_string(),
        Some(parent),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("main added file".to_string()),
        vec![FileChange {
            path: path.to_string(),
            change_type: ChangeType::Added,
            old_blob_hash: None,
            new_blob_hash: Some(new_blob),
            old_path: None,
            old_mode: None,
            new_mode: Some(FileMode::Regular),
        }],
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    repo.set_branch_head("main", &commit.hash).unwrap();
}

fn advance_main_deleting_tracked_txt(dir: &Path, parent: oak_core::Hash) {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let parent_commit = repo.get_commit(&parent).unwrap().unwrap();
    let parent_manifest = repo
        .get_manifest(&parent_commit.manifest_hash)
        .unwrap()
        .unwrap();
    let old_entry = parent_manifest.get("tracked.txt").unwrap();
    let entries: Vec<ManifestEntry> = parent_manifest
        .entries
        .iter()
        .filter(|entry| entry.path != "tracked.txt")
        .cloned()
        .collect();
    let manifest = Manifest::new(entries);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "main".to_string(),
        Some(parent),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("main deleted tracked".to_string()),
        vec![FileChange {
            path: "tracked.txt".to_string(),
            change_type: ChangeType::Deleted,
            old_blob_hash: Some(old_entry.blob_hash.clone()),
            new_blob_hash: None,
            old_path: None,
            old_mode: Some(FileMode::Regular),
            new_mode: None,
        }],
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    repo.set_branch_head("main", &commit.hash).unwrap();
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

fn seed_remote_feature_commit_with_missing_blob(
    dir: &Path,
    parent: oak_core::Hash,
) -> oak_core::Hash {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let parent_commit = repo.get_commit(&parent).unwrap().unwrap();
    let parent_manifest = repo
        .get_manifest(&parent_commit.manifest_hash)
        .unwrap()
        .unwrap();
    let old_entry = parent_manifest.get("tracked.txt").unwrap();
    let missing_blob = oak_core::Hash("f9".repeat(32));
    let manifest = Manifest::new(vec![ManifestEntry {
        path: "tracked.txt".to_string(),
        blob_hash: missing_blob.clone(),
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "remote-feature".to_string(),
        Some(parent),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("remote branch metadata without blob".to_string()),
        vec![FileChange {
            path: "tracked.txt".to_string(),
            change_type: ChangeType::Modified,
            old_blob_hash: Some(old_entry.blob_hash.clone()),
            new_blob_hash: Some(missing_blob),
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
    repo.set_metadata(MetadataKey::ApiKey, "test-token")
        .unwrap();
}

fn credential_bearing_remote(server: &MockServer) -> String {
    let with_userinfo =
        server
            .uri()
            .replacen("http://", "http://QA_LEGACY_USER:QA_LEGACY_PASSWORD@", 1);
    format!("{with_userinfo}/?access_token=QA_LEGACY_QUERY#QA_LEGACY_FRAGMENT")
}

fn assert_legacy_remote_secrets_absent(value: &str) {
    for secret in [
        "QA_LEGACY_USER",
        "QA_LEGACY_PASSWORD",
        "QA_LEGACY_QUERY",
        "QA_LEGACY_FRAGMENT",
    ] {
        assert!(!value.contains(secret), "output leaked {secret}: {value}");
    }
}

fn stored_remote(dir: &Path) -> Option<String> {
    SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_metadata(MetadataKey::RemoteUrl)
        .unwrap()
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

    // The append-only `changes` alias remains, while the authoritative sets
    // distinguish uncommitted work from the committed branch contribution.
    assert_eq!(json["working_changes"]["base"], json["head"]);
    assert_eq!(json["working_changes"]["changes"], json["changes"]);
    let branch_changes = json["branch_changes"]["changes"].as_array().unwrap();
    assert!(branch_changes
        .iter()
        .any(|change| change["path"] == "tracked.txt" && change["status"] == "added"));
    assert!(!branch_changes
        .iter()
        .any(|change| change["path"] == "new.txt"));
    assert_eq!(json["branch_changes"]["head"], json["head"]);
}

#[test]
fn status_json_labels_committed_changes_with_the_exact_fork_and_head() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    std::fs::write(dir.join("tracked.txt"), "committed branch work\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let json = json_stdout(&oak(dir, &["status", "--json"]));

    assert_eq!(json["branch_changes"]["base"], base.as_str());
    assert_eq!(json["branch_changes"]["head"], json["head"]);
    assert!(json["working_changes"]["changes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(json["branch_changes"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change["path"] == "tracked.txt" && change["status"] == "modified"));
}

#[test]
fn status_json_keeps_working_status_when_branch_base_is_not_hydrated() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    std::fs::write(dir.join("tracked.txt"), "committed branch work\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("new.txt"), "uncommitted work\n").unwrap();

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let base_commit = repo.get_commit(&base).unwrap().unwrap();
    drop(repo);
    rusqlite::Connection::open(dir.join(".oak/oak.db"))
        .unwrap()
        .execute(
            "UPDATE trees SET content = X'00' WHERE hash = ?1",
            rusqlite::params![base_commit.manifest_hash.as_str()],
        )
        .unwrap();

    let json = json_stdout(&oak(dir, &["status", "--json"]));

    assert!(json["working_changes"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change["path"] == "new.txt"));
    assert!(json["branch_changes"]["changes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(json["branch_changes"]["caveats"][0]
        .as_str()
        .unwrap()
        .contains("unavailable"));
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
    repo.set_metadata(MetadataKey::ApiKey, "test-token")
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
fn conflict_take_ours_errors_when_content_contains_ambiguous_separator_line() {
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

    let before = std::fs::read_to_string(dir.join("tracked.txt")).unwrap();
    let out = oak(dir, &["conflict", "take", "tracked.txt", "--ours"]);

    assert!(!out.status.success());
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        before,
        "malformed marker file must not be overwritten"
    );
    assert!(
        stderr(&out).contains("malformed conflict markers"),
        "expected malformed marker error, got stderr:\n{}",
        stderr(&out)
    );
}

#[test]
fn conflict_take_ours_json_errors_when_content_contains_ambiguous_theirs_marker_line() {
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

    let before = std::fs::read_to_string(dir.join("tracked.txt")).unwrap();
    let out = oak(
        dir,
        &["conflict", "take", "tracked.txt", "--ours", "--json"],
    );

    assert!(!out.status.success());
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        before,
        "malformed marker file must not be overwritten"
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("malformed conflict markers"),
        "{json}"
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

    assert_eq!(json["schema_version"], AGENT_STATE_SCHEMA_VERSION);
    assert_eq!(json["context"], "checkout");
    assert!(json["branch"].as_str().unwrap().starts_with("tester-"));
    assert_eq!(json["dirty"], true);
    assert_eq!(json["changes"][0]["path"], "tracked.txt");
    assert_eq!(json["recommended_next_commands"][0], "oak commit");
    assert_eq!(json["recommended_action"]["kind"], "commit");
    assert_eq!(json["recommended_action"]["command"], "oak commit");
    assert_eq!(json["recommended_action"]["mutates"], true);
    assert_eq!(json["recommended_action"]["needs_network"], false);
    assert_eq!(json["recommended_action"]["confidence"], "high");
    assert_eq!(
        json["recommended_action"]["remote_freshness"],
        "not_configured"
    );
    assert_eq!(
        json["recommended_action"]["blocking_reason"],
        "no_remote_configured"
    );
    assert!(json["recommended_action"]["risk_notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note == "commit_is_local_checkpoint_only"));
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
    // Full agent-state schema v2 uses `finish_eligible`; the v1
    // `can_finish` alias is intentionally no longer emitted.
    assert!(json.get("can_finish").is_none());
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
    assert_eq!(compact["recommended_action"]["kind"], "commit");
    assert_eq!(compact["recommended_action"]["command"], "oak commit");
    assert_eq!(compact["recommended_action"]["needs_network"], false);

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
    repo.set_metadata(MetadataKey::ApiKey, "test-token")
        .unwrap();
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    let commands = json["recommended_next_commands"].as_array().unwrap();
    assert_eq!(commands[0], "oak finish --desc-file <file> --json");
    assert!(commands.iter().any(|cmd| cmd == "oak commit --push"));
    assert!(commands.iter().any(|cmd| cmd == "oak push"));
    assert_eq!(json["recommended_action"]["kind"], "finish");
    assert_eq!(
        json["recommended_action"]["command"],
        "oak finish --desc-file <file> --json"
    );
    assert_eq!(json["recommended_action"]["needs_network"], true);
    assert_eq!(json["recommended_action"]["confidence"], "medium");
    assert!(json["recommended_action"]["risk_notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note == "finish_will_commit_and_push"));
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
    repo.set_metadata(MetadataKey::RemoteUrl, &credential_bearing_remote(&server))
        .unwrap();
    repo.set_metadata(MetadataKey::ApiKey, "refresh-bearer-token")
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/agent-json"))
        .and(header("authorization", "Bearer refresh-bearer-token"))
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
        .and(header("authorization", "Bearer refresh-bearer-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": head
        })))
        .mount(&server)
        .await;

    let output = oak(dir, &["agent", "state", "--json", "--refresh"]);
    let json = json_stdout(&output);

    assert_eq!(json["schema_version"], AGENT_STATE_SCHEMA_VERSION);
    assert_eq!(json["current_branch_pushed_head"], head);
    assert_eq!(json["current_branch_push_checked"], true);
    assert_eq!(json["refresh_requested"], true);
    assert_eq!(json["refresh_supported"], true);
    assert_eq!(json["refresh_errors"].as_array().unwrap().len(), 0);
    assert_eq!(json["needs_push"], false);
    assert_eq!(json["unpushed_commit_count"], 0);
    assert_eq!(json["remote_url"], server.uri());
    assert_legacy_remote_secrets_absent(&stdout(&output));
    assert_legacy_remote_secrets_absent(&stderr(&output));
    let receipt = std::fs::read_to_string(dir.join(".oak/LAST_PUSHED_HEAD.json")).unwrap();
    assert_legacy_remote_secrets_absent(&receipt);
    assert_eq!(
        serde_json::from_str::<Value>(&receipt).unwrap()["remote"],
        server.uri()
    );
    assert_eq!(stored_remote(dir).as_deref(), Some(server.uri().as_str()));
    for request in server.received_requests().await.unwrap() {
        assert_eq!(request.url.query(), None);
        assert_legacy_remote_secrets_absent(request.url.as_str());
    }
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
    repo.set_metadata(MetadataKey::RemoteUrl, &credential_bearing_remote(&server))
        .unwrap();
    repo.set_metadata(MetadataKey::ApiKey, "refresh-bearer-token")
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/agent-json"))
        .and(header("authorization", "Bearer refresh-bearer-token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/agent-json/branches/{branch}")))
        .and(header("authorization", "Bearer refresh-bearer-token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let output = oak(dir, &["agent", "state", "--json", "--refresh"]);
    let json = json_stdout(&output);

    assert_eq!(json["schema_version"], AGENT_STATE_SCHEMA_VERSION);
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
    assert_eq!(json["remote_url"], server.uri());
    assert_legacy_remote_secrets_absent(&stdout(&output));
    assert_legacy_remote_secrets_absent(&stderr(&output));
    assert!(!dir.join(".oak/LAST_PUSHED_HEAD.json").exists());
    assert_eq!(stored_remote(dir).as_deref(), Some(server.uri().as_str()));
    for request in server.received_requests().await.unwrap() {
        assert_eq!(request.url.query(), None);
        assert_legacy_remote_secrets_absent(request.url.as_str());
    }
}

#[tokio::test]
async fn agent_state_refresh_persists_sanitized_deleted_branch_tombstone() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);
    let head = seed_local_main_from_current_head(dir).to_string();
    let server = MockServer::start().await;
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "agent-json")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, &credential_bearing_remote(&server))
        .unwrap();
    repo.set_metadata(MetadataKey::ApiKey, "refresh-bearer-token")
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/agent-json"))
        .and(header("authorization", "Bearer refresh-bearer-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": head,
            "name": "agent-json",
            "is_public": true,
            "owner": "oak"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/agent-json/branches/{branch}")))
        .and(header("authorization", "Bearer refresh-bearer-token"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let refreshed = oak(dir, &["agent", "state", "--json", "--refresh"]);
    let refreshed_json = json_stdout(&refreshed);
    assert_eq!(refreshed_json["needs_push"], true);
    assert_eq!(refreshed_json["remote_url"], server.uri());
    assert_legacy_remote_secrets_absent(&stdout(&refreshed));
    assert_legacy_remote_secrets_absent(&stderr(&refreshed));

    let receipt = std::fs::read_to_string(dir.join(".oak/LAST_PUSHED_HEAD.json")).unwrap();
    assert_legacy_remote_secrets_absent(&receipt);
    let receipt_json: Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(receipt_json["remote"], server.uri());
    assert_eq!(receipt_json["head"], Value::Null);
    assert_eq!(receipt_json["source"], "remote_refresh_cache");
    assert_eq!(stored_remote(dir).as_deref(), Some(server.uri().as_str()));
    for request in server.received_requests().await.unwrap() {
        assert_eq!(request.url.query(), None);
        assert_legacy_remote_secrets_absent(request.url.as_str());
    }

    let offline = oak(dir, &["agent", "state", "--json"]);
    let offline_json = json_stdout(&offline);
    assert_eq!(offline_json["needs_push"], true);
    assert_eq!(
        offline_json["current_branch_push_source"],
        "remote_refresh_cache"
    );
    assert_eq!(offline_json["remote_url"], server.uri());
    assert_legacy_remote_secrets_absent(&stdout(&offline));
    assert_legacy_remote_secrets_absent(&stderr(&offline));
}

#[test]
fn agent_state_json_reports_finish_eligibility() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    // Link a remote and token so this clean checkout is genuinely finish-eligible.
    // Without credentials, `oak finish` would fail its preflight before mutating.
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "agent-json")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.example")
        .unwrap();
    repo.set_metadata(MetadataKey::ApiKey, "test-token")
        .unwrap();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["schema_version"], AGENT_STATE_SCHEMA_VERSION);
    assert_eq!(json["context"], "checkout");
    assert_eq!(json["dirty"], false);
    assert_eq!(json["unpushed_commit_count"], 0);
    assert_eq!(json["blocking_reason"], Value::Null);
    assert!(json.get("can_finish").is_none());
    assert_eq!(json["finish_eligible"], true);
    assert_eq!(
        json["recommended_next_commands"][0],
        "oak finish --desc-file <file> --json"
    );
    assert_eq!(json["recommended_action"]["kind"], "finish");
    assert_eq!(
        json["recommended_action"]["command"],
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
    assert!(json.get("can_finish").is_none());
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
fn agent_state_json_uses_repo_placeholder_for_unlinked_publish_action() {
    let temp = fixture_repo();
    let dir = temp.path();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["dirty"], false);
    assert_eq!(json["needs_push"], true);
    assert_eq!(json["blocking_reason"], "no_remote_configured");
    assert_eq!(
        json["recommended_next_commands"][0],
        "oak push --repo <org>/<repo>"
    );
    assert_eq!(json["recommended_action"]["kind"], "push");
    assert_eq!(
        json["recommended_action"]["command"],
        "oak push --repo <org>/<repo>"
    );
    assert_eq!(json["recommended_action"]["needs_network"], true);
    assert_eq!(
        json["recommended_action"]["blocking_reason"],
        "no_remote_configured"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn agent_state_json_blocks_finish_for_closed_current_branch() {
    let temp = fixture_repo();
    let dir = temp.path();
    let server = MockServer::start().await;
    link_remote(dir, &server);

    let branch = current_branch(dir);
    {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.close_branch(&branch, None).unwrap();
    }

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["finish_eligible"], false);
    assert_eq!(json["blocking_reason"], "branch_closed");
    assert!(!json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd.as_str().unwrap().starts_with("oak finish")));
    assert_eq!(
        json["recommended_action"]["blocking_reason"],
        "branch_closed"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn agent_state_json_blocks_finish_for_linked_checkout_without_auth() {
    let temp = fixture_repo();
    let dir = temp.path();
    let server = MockServer::start().await;
    {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "agent-json")
            .unwrap();
    }
    std::fs::write(dir.join("tracked.txt"), "changed without auth\n").unwrap();

    let json = json_stdout(&oak(dir, &["agent", "state", "--json"]));

    assert_eq!(json["dirty"], true);
    assert_eq!(json["finish_eligible"], false);
    assert_eq!(json["blocking_reason"], "auth_missing");
    let commands = json["recommended_next_commands"].as_array().unwrap();
    assert_eq!(commands[0], format!("oak login -r {}", server.uri()));
    assert!(!commands
        .iter()
        .any(|cmd| cmd.as_str().unwrap().starts_with("oak finish")));
}

#[tokio::test(flavor = "current_thread")]
async fn finish_desc_file_json_finishes_clean_unpushed_zero_branch() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    let branch_name = current_branch(dir);
    let server = MockServer::start().await;
    {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "agent-json")
            .unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        repo.set_metadata(MetadataKey::ApiKey, "test-token")
            .unwrap();
    }
    Mock::given(method("GET"))
        .and(path("/api/oak/agent-json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/agent-json/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;
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
    assert_eq!(json["branch"], branch_name);
    assert_eq!(json["branch_description"], "Ship final task\n\nDetails");
    assert_eq!(json["phase"], "complete");
    assert_eq!(
        json["completed_phases"],
        serde_json::json!(["preflight", "description", "metadata_sync"])
    );
    assert_eq!(json["pending_phases"], serde_json::json!([]));
    assert_eq!(json["retry_command"], Value::Null);
    assert_eq!(json["manual_recovery_commands"], serde_json::json!([]));
    assert_eq!(json["committed"], false);
    assert_eq!(json["pushed"], false);
    assert_eq!(json["description_synced"], true);
    assert_eq!(json["unpushed_before"], 0);
    assert_eq!(json["unpushed_after"], 0);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "finish should preflight the remote repo before metadata sync"
    );
}

#[test]
fn finish_json_preflight_failure_leaves_description_dirty_tree_and_head_unchanged() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(oak(dir, &["desc", "before finish"]).status.success());
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch = current_branch(dir);
    let head_before = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit_count_before = repo.count_commits_for_branch(&branch).unwrap();
    std::fs::write(dir.join("tracked.txt"), "dirty before failed finish\n").unwrap();
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "new finish description").unwrap();

    let out = oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(!out.status.success());
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "finish_preflight_failed");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("requires a linked remote"));
    assert_eq!(json["error"]["finish"]["phase"], "preflight");
    assert_eq!(json["error"]["finish"]["blocker"], "remote_not_configured");
    assert_eq!(
        json["error"]["finish"]["completed_phases"],
        serde_json::json!([])
    );
    assert_eq!(
        json["error"]["finish"]["pending_phases"],
        serde_json::json!(["description", "commit", "push", "metadata_sync"])
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch_after = repo.get_branch(&branch).unwrap().unwrap();
    assert_eq!(branch_after.description.as_deref(), Some("before finish"));
    assert_eq!(repo.get_branch_head(&branch).unwrap().unwrap(), head_before);
    assert_eq!(
        repo.count_commits_for_branch(&branch).unwrap(),
        commit_count_before
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "dirty before failed finish\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_json_auth_preflight_leaves_description_dirty_tree_and_head_unchanged() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(oak(dir, &["desc", "before auth finish"]).status.success());
    let server = MockServer::start().await;
    {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "agent-json")
            .unwrap();
    }
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch = current_branch(dir);
    let head_before = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit_count_before = repo.count_commits_for_branch(&branch).unwrap();
    std::fs::write(dir.join("tracked.txt"), "dirty before auth failed finish\n").unwrap();
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "new finish description").unwrap();

    let out = oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "finish_preflight_failed");
    assert_eq!(json["error"]["finish"]["phase"], "preflight");
    assert_eq!(json["error"]["finish"]["blocker"], "auth_missing");
    assert_eq!(
        json["error"]["finish"]["retry_command"],
        format!("oak login -r {}", server.uri())
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "auth preflight must fail before contacting the remote"
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch_after = repo.get_branch(&branch).unwrap().unwrap();
    assert_eq!(
        branch_after.description.as_deref(),
        Some("before auth finish")
    );
    assert_eq!(repo.get_branch_head(&branch).unwrap().unwrap(), head_before);
    assert_eq!(
        repo.count_commits_for_branch(&branch).unwrap(),
        commit_count_before
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "dirty before auth failed finish\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_json_remote_preflight_leaves_description_dirty_tree_and_head_unchanged() {
    let temp = fixture_repo();
    let dir = temp.path();
    assert!(oak(dir, &["desc", "before remote finish"]).status.success());
    let server = MockServer::start().await;
    link_remote(dir, &server);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(503).set_body_string("temporarily unavailable"))
        .expect(1)
        .mount(&server)
        .await;

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch = current_branch(dir);
    let head_before = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit_count_before = repo.count_commits_for_branch(&branch).unwrap();
    drop(repo);
    std::fs::write(
        dir.join("tracked.txt"),
        "dirty before remote failed finish\n",
    )
    .unwrap();
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "new finish description").unwrap();

    let out = oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "finish_preflight_failed");
    assert_eq!(json["error"]["finish"]["phase"], "preflight");
    assert_eq!(json["error"]["finish"]["blocker"], "remote_unreachable");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("before mutating local state"),
        "expected remote preflight message, got: {json}"
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch_after = repo.get_branch(&branch).unwrap().unwrap();
    assert_eq!(
        branch_after.description.as_deref(),
        Some("before remote finish")
    );
    assert_eq!(repo.get_branch_head(&branch).unwrap().unwrap(), head_before);
    assert_eq!(
        repo.count_commits_for_branch(&branch).unwrap(),
        commit_count_before
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "dirty before remote failed finish\n"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "finish should perform only the read-only remote preflight"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_env_remote_overrides_stored_remote_without_rewriting_it() {
    let temp = fixture_repo();
    let dir = temp.path();
    let stored_server = MockServer::start().await;
    let env_server = MockServer::start().await;
    link_remote(dir, &stored_server);
    let stored_url = stored_server.uri();
    let env_url = env_server.uri();

    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(503).set_body_string("env remote down"))
        .expect(1)
        .mount(&env_server)
        .await;

    let out = oak_with_env(dir, &["push"], &[("OAK_REMOTE", env_url.as_str())]);

    assert!(!out.status.success());
    assert!(
        !env_server.received_requests().await.unwrap().is_empty(),
        "push should contact OAK_REMOTE"
    );
    assert_eq!(
        stored_server.received_requests().await.unwrap().len(),
        0,
        "push must not contact the stored remote when OAK_REMOTE is set"
    );
    assert_eq!(
        stored_remote(dir).as_deref(),
        Some(stored_url.as_str()),
        "one-shot OAK_REMOTE must not rewrite linked checkout metadata"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pull_env_remote_overrides_stored_remote_without_rewriting_it() {
    let temp = fixture_repo();
    let dir = temp.path();
    let stored_server = MockServer::start().await;
    let env_server = MockServer::start().await;
    link_remote(dir, &stored_server);
    let stored_url = stored_server.uri();
    let env_url = env_server.uri();

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(503).set_body_string("env remote down"))
        .expect(1)
        .mount(&env_server)
        .await;

    let out = oak_with_env(dir, &["pull"], &[("OAK_REMOTE", env_url.as_str())]);

    assert!(!out.status.success());
    assert!(
        !env_server.received_requests().await.unwrap().is_empty(),
        "pull should contact OAK_REMOTE"
    );
    assert_eq!(
        stored_server.received_requests().await.unwrap().len(),
        0,
        "pull must not contact the stored remote when OAK_REMOTE is set"
    );
    assert_eq!(
        stored_remote(dir).as_deref(),
        Some(stored_url.as_str()),
        "pull must not persist one-shot OAK_REMOTE"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_env_remote_overrides_stored_remote_without_rewriting_it() {
    let temp = fixture_repo();
    let dir = temp.path();
    let stored_server = MockServer::start().await;
    let env_server = MockServer::start().await;
    link_remote(dir, &stored_server);
    let stored_url = stored_server.uri();
    let env_url = env_server.uri();

    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(503).set_body_string("env remote down"))
        .expect(1)
        .mount(&env_server)
        .await;

    let out = oak_with_env(dir, &["fetch"], &[("OAK_REMOTE", env_url.as_str())]);

    assert!(!out.status.success());
    assert!(
        !env_server.received_requests().await.unwrap().is_empty(),
        "fetch should contact OAK_REMOTE"
    );
    assert_eq!(
        stored_server.received_requests().await.unwrap().len(),
        0,
        "fetch must not contact the stored remote when OAK_REMOTE is set"
    );
    assert_eq!(
        stored_remote(dir).as_deref(),
        Some(stored_url.as_str()),
        "fetch must not persist one-shot OAK_REMOTE"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pull_env_remote_links_unlinked_checkout_on_first_use() {
    let temp = fixture_repo();
    let dir = temp.path();
    let env_server = MockServer::start().await;
    let env_url = env_server.uri();
    {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
        repo.set_metadata(MetadataKey::ApiKey, "test-token")
            .unwrap();
    }

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(503).set_body_string("env remote down"))
        .expect(1)
        .mount(&env_server)
        .await;

    let out = oak_with_env(dir, &["pull"], &[("OAK_REMOTE", env_url.as_str())]);

    assert!(!out.status.success());
    assert!(
        !env_server.received_requests().await.unwrap().is_empty(),
        "pull should contact OAK_REMOTE"
    );
    assert_eq!(
        stored_remote(dir).as_deref(),
        Some(env_url.as_str()),
        "first-use OAK_REMOTE should link an unlinked checkout"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn commit_push_env_remote_matches_bare_push_precedence_without_rewriting_metadata() {
    let temp = fixture_repo();
    let dir = temp.path();
    let stored_server = MockServer::start().await;
    let env_server = MockServer::start().await;
    link_remote(dir, &stored_server);
    let stored_url = stored_server.uri();
    let env_url = env_server.uri();
    std::fs::write(dir.join("tracked.txt"), "commit push env override\n").unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(503).set_body_string("env remote down"))
        .expect(1)
        .mount(&env_server)
        .await;

    let out = oak_with_env(
        dir,
        &["commit", "--push", "--json", "--quiet"],
        &[("OAK_REMOTE", env_url.as_str())],
    );

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(6));
    assert!(
        !env_server.received_requests().await.unwrap().is_empty(),
        "commit --push should contact OAK_REMOTE"
    );
    assert_eq!(
        stored_server.received_requests().await.unwrap().len(),
        0,
        "commit --push must not contact the stored remote when OAK_REMOTE is set"
    );
    assert_eq!(
        stored_remote(dir).as_deref(),
        Some(stored_url.as_str()),
        "commit --push must not persist one-shot OAK_REMOTE"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_blank_remote_is_rejected_before_defaulting_or_env_fallback() {
    let temp = fixture_repo();
    let dir = temp.path();
    let env_server = MockServer::start().await;
    let env_url = env_server.uri();

    for args in [
        vec!["whoami", "-r", "   "],
        vec!["site", "list", "--remote", "   "],
        vec!["clone", "-r", "   ", "oak/oak", "blank-remote-clone"],
    ] {
        let out = oak_with_env(dir, &args, &[("OAK_REMOTE", env_url.as_str())]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "args {args:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("remote URL cannot be empty"),
            "args {args:?}\nstderr:\n{}",
            stderr(&out)
        );
    }

    assert_eq!(
        env_server.received_requests().await.unwrap().len(),
        0,
        "blank explicit remotes must not fall through to OAK_REMOTE"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn commit_push_json_auth_missing_recommends_login_command() {
    let temp = fixture_repo();
    let dir = temp.path();
    let server = MockServer::start().await;
    let branch_name = current_branch(dir);
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let head_before = repo.get_branch_head(&branch_name).unwrap().unwrap();
    let commit_count_before = repo.count_commits_for_branch(&branch_name).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    drop(repo);
    std::fs::write(dir.join("tracked.txt"), "commit push without auth\n").unwrap();

    let out = oak(dir, &["commit", "--push", "--json", "--quiet"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "configuration_error");
    assert_eq!(
        json["error"]["recommended_next_commands"][0],
        format!("oak login -r {}", server.uri())
    );
    assert!(json["error"]["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd == "oak info --json"));
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_branch_head(&branch_name).unwrap().unwrap(),
        head_before
    );
    assert_eq!(
        repo.count_commits_for_branch(&branch_name).unwrap(),
        commit_count_before,
        "auth preflight must fail before creating a local checkpoint"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "auth preflight must fail before contacting the remote"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_env_remote_overrides_stored_remote_without_rewriting_it() {
    let temp = fixture_repo();
    let dir = temp.path();
    let stored_server = MockServer::start().await;
    let env_server = MockServer::start().await;
    link_remote(dir, &stored_server);
    let stored_url = stored_server.uri();
    let env_url = env_server.uri();
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(503).set_body_string("env remote down"))
        .expect(1)
        .mount(&env_server)
        .await;
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "finish env override").unwrap();

    let out = oak_with_env(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
        &[("OAK_REMOTE", env_url.as_str())],
    );

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(6));
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["finish"]["blocker"], "remote_unreachable");
    assert_eq!(
        env_server.received_requests().await.unwrap().len(),
        1,
        "finish should preflight OAK_REMOTE"
    );
    assert_eq!(
        stored_server.received_requests().await.unwrap().len(),
        0,
        "finish must not contact the stored remote when OAK_REMOTE is set"
    );
    assert_eq!(
        stored_remote(dir).as_deref(),
        Some(stored_url.as_str()),
        "finish must not persist one-shot OAK_REMOTE"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_missing_remote_repo_preflight_exits_retryable() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    let server = MockServer::start().await;
    link_remote(dir, &server);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "finish missing remote repo").unwrap();

    let out = oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(6));
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["finish"]["blocker"], "remote_repo_missing");
}

#[tokio::test(flavor = "current_thread")]
async fn finish_json_allows_missing_repo_preflight_when_push_will_create() {
    let temp = fixture_repo();
    let dir = temp.path();
    let head = SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_branch_head(&current_branch(dir))
        .unwrap()
        .unwrap()
        .to_string();
    let server = MockServer::start().await;
    link_remote(dir, &server);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/oak/branches/{}",
            current_branch(dir)
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": head,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "finish with first publish").unwrap();

    let out = oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    let json = json_stdout(&out);
    assert_eq!(json["phase"], "complete");
    assert_eq!(json["committed"], false);
    assert_eq!(json["pushed"], true);
    assert_eq!(json["description_synced"], true);
}

#[tokio::test(flavor = "current_thread")]
async fn finish_json_preflight_follows_trusted_remote_move() {
    let temp = fixture_repo();
    let dir = temp.path();
    let head = SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_branch_head(&current_branch(dir))
        .unwrap()
        .unwrap()
        .to_string();
    let old_server = MockServer::start().await;
    let new_server = MockServer::start().await;
    link_remote(dir, &old_server);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/api/oak/oak", new_server.uri()).as_str(),
        ))
        .expect(1)
        .mount(&old_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(2)
        .mount(&new_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/oak/branches/{}",
            current_branch(dir)
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&new_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": head,
            "message": "ok"
        })))
        .expect(1)
        .mount(&new_server)
        .await;
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "finish after remote move").unwrap();

    let out = oak_with_env(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
        &[("OAK_TRUSTED_REMOTES", new_server.uri().as_str())],
    );

    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    let json = json_stdout(&out);
    assert_eq!(json["phase"], "complete");
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_metadata(MetadataKey::RemoteUrl)
            .unwrap()
            .as_deref(),
        Some(new_server.uri().as_str())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_json_push_phase_failure_reports_retryable_mutated_state() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);
    let head_before = SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_branch_head(&branch)
        .unwrap()
        .unwrap();
    let server = MockServer::start().await;
    link_remote(dir, &server);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(503).set_body_string("temporarily unavailable"))
        .expect(1)
        .mount(&server)
        .await;
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "finish desc before push outage").unwrap();

    let out = oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(!out.status.success());
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "finish_phase_failed");
    assert_eq!(json["error"]["finish"]["phase"], "push");
    assert_eq!(
        json["error"]["finish"]["completed_phases"],
        serde_json::json!(["preflight", "description"])
    );
    assert_eq!(
        json["error"]["finish"]["pending_phases"],
        serde_json::json!(["push", "metadata_sync"])
    );
    assert_eq!(json["error"]["finish"]["retry_command"], "oak push");
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch_after = repo.get_branch(&branch).unwrap().unwrap();
    assert_eq!(
        branch_after.description.as_deref(),
        Some("finish desc before push outage"),
        "description mutation should be preserved for idempotent retry"
    );
    assert_eq!(
        repo.get_branch_head(&branch).unwrap().as_ref(),
        Some(&head_before),
        "push phase failure must not create another local commit"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_json_metadata_sync_failure_reports_retryable_description_state() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    let branch = current_branch(dir);
    let server = MockServer::start().await;
    {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        repo.set_metadata(MetadataKey::ApiKey, "test-token")
            .unwrap();
    }
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(503).set_body_string("temporarily unavailable"))
        .expect(1)
        .mount(&server)
        .await;
    let desc_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(desc_file.path(), "finish desc before metadata outage").unwrap();

    let out = oak(
        dir,
        &[
            "finish",
            "--desc-file",
            desc_file.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(!out.status.success());
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "finish_phase_failed");
    assert_eq!(json["error"]["finish"]["phase"], "metadata_sync");
    assert_eq!(
        json["error"]["finish"]["completed_phases"],
        serde_json::json!(["preflight", "description"])
    );
    assert_eq!(
        json["error"]["finish"]["pending_phases"],
        serde_json::json!(["metadata_sync"])
    );
    assert_eq!(
        json["error"]["finish"]["retry_command"],
        "oak finish --desc-file <file> --json"
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch_after = repo.get_branch(&branch).unwrap().unwrap();
    assert_eq!(
        branch_after.description.as_deref(),
        Some("finish desc before metadata outage"),
        "local description should survive so retry is idempotent"
    );
    assert_eq!(
        repo.count_commits_for_branch(&branch).unwrap(),
        0,
        "metadata-only finish failure must not create a local commit"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn commit_linked_repo_without_push_does_not_contact_remote() {
    let temp = fixture_repo();
    let dir = temp.path();
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let head_before = repo
        .get_branch_head(&current_branch(dir))
        .unwrap()
        .unwrap()
        .to_string();
    let server = MockServer::start().await;
    link_remote(dir, &server);
    std::fs::write(dir.join("tracked.txt"), "local checkpoint\n").unwrap();

    let out = oak(dir, &["commit"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let head_after = repo
        .get_branch_head(&current_branch(dir))
        .unwrap()
        .unwrap()
        .to_string();
    assert_ne!(head_before, head_after);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "plain commit must not contact the linked remote"
    );
}

#[test]
fn commit_json_quiet_reports_local_checkpoint_without_human_output() {
    let temp = fixture_repo();
    let dir = temp.path();
    let head_before = SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_branch_head(&current_branch(dir))
        .unwrap()
        .unwrap()
        .to_string();
    std::fs::write(dir.join("tracked.txt"), "json checkpoint\n").unwrap();

    let out = oak(dir, &["commit", "--json", "--quiet"]);

    assert_eq!(stderr(&out), "");
    let json = json_stdout(&out);
    assert_eq!(json["committed"], true);
    assert_eq!(json["pushed"], false);
    assert_eq!(json["published"], false);
    assert_eq!(json["remote_contacted"], false);
    assert_eq!(json["head_before"], head_before);
    assert_ne!(json["head_after"], head_before);
    assert_eq!(json["change_counts"]["modified"], 1);
    assert_eq!(json["paths_sample"], serde_json::json!(["tracked.txt"]));
    assert_eq!(json["unpushed_commit_count"], 2);
    assert_eq!(json["next_commands"][0], "oak push --repo <org>/<repo>");
}

#[test]
fn commit_json_failure_uses_structured_error_envelope() {
    let temp = tempfile::TempDir::new().unwrap();

    let out = oak(temp.path(), &["commit", "--json", "--quiet"]);

    assert!(!out.status.success());
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "repo_not_found");
    assert_eq!(json["error"]["recommended_next_commands"][0], "oak init");
}

#[test]
fn commit_push_json_quiet_unlinked_preflight_does_not_commit() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let head_before = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit_count_before = repo.count_commits_for_branch(&branch).unwrap();
    drop(repo);
    std::fs::write(dir.join("tracked.txt"), "would need publish setup\n").unwrap();

    let out = oak(dir, &["commit", "--push", "--json", "--quiet"]);

    assert!(!out.status.success());
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "configuration_error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not linked"),
        "expected link preflight error, got: {json}"
    );
    assert_eq!(
        json["error"]["recommended_next_commands"][0],
        "oak push --repo <org>/<repo>"
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_branch_head(&branch).unwrap().as_ref(),
        Some(&head_before),
        "failed publish preflight must not move HEAD"
    );
    assert_eq!(
        repo.count_commits_for_branch(&branch).unwrap(),
        commit_count_before,
        "failed publish preflight must not create a commit"
    );
}

#[test]
fn commit_push_json_quiet_rejects_default_remote_source() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);
    {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
        repo.set_metadata(MetadataKey::ApiKey, "test-token")
            .unwrap();
    }
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let head_before = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit_count_before = repo.count_commits_for_branch(&branch).unwrap();
    drop(repo);
    std::fs::write(
        dir.join("tracked.txt"),
        "would otherwise publish to default\n",
    )
    .unwrap();

    let out = oak(dir, &["commit", "--push", "--json", "--quiet"]);

    assert!(!out.status.success());
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "configuration_error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not linked"),
        "expected link preflight error, got: {json}"
    );
    assert_eq!(
        json["error"]["recommended_next_commands"][0],
        "oak push --repo <org>/<repo>"
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(stored_remote(dir), None);
    assert_eq!(
        repo.get_branch_head(&branch).unwrap().as_ref(),
        Some(&head_before),
        "default-remote preflight must not move HEAD"
    );
    assert_eq!(
        repo.count_commits_for_branch(&branch).unwrap(),
        commit_count_before,
        "default-remote preflight must not create a commit"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn commit_push_json_quiet_contacts_remote_and_reports_publish() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch_name = current_branch(dir);
    let server = MockServer::start().await;
    link_remote(dir, &server);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
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
    std::fs::write(dir.join("tracked.txt"), "publish checkpoint\n").unwrap();

    let out = oak(dir, &["commit", "--push", "--json", "--quiet"]);

    assert_eq!(stderr(&out), "");
    let json = json_stdout(&out);
    assert_eq!(json["committed"], true);
    assert_eq!(json["pushed"], true);
    assert_eq!(json["published"], true);
    assert_eq!(json["remote_contacted"], true);
    assert_eq!(json["unpushed_commit_count"], 0);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "commit --push should perform the push protocol once"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn commit_push_json_quiet_push_failure_reports_committed_checkpoint() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch_name = current_branch(dir);
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let head_before = repo.get_branch_head(&branch_name).unwrap().unwrap();
    drop(repo);

    let server = MockServer::start().await;
    link_remote(dir, &server);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(503).set_body_string("temporarily unavailable"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    std::fs::write(dir.join("tracked.txt"), "checkpoint before remote outage\n").unwrap();

    let out = oak(dir, &["commit", "--push", "--json", "--quiet"]);

    assert_eq!(out.status.code(), Some(6));
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(json["error"]["code"], "commit_phase_failed");
    assert_eq!(json["error"]["commit"]["phase"], "push");
    assert_eq!(json["error"]["commit"]["committed"], true);
    assert_eq!(json["error"]["commit"]["pushed"], false);
    assert_eq!(json["error"]["commit"]["published"], false);
    assert_eq!(json["error"]["commit"]["remote_contacted"], true);
    assert_eq!(json["error"]["commit"]["branch"], branch_name);
    assert_eq!(json["error"]["commit"]["head_before"], head_before.as_str());
    assert_ne!(json["error"]["commit"]["head_after"], head_before.as_str());
    assert_eq!(json["error"]["commit"]["retry_command"], "oak push");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("commit saved locally"),
        "expected phase-specific message, got: {json}"
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_branch_head(&branch_name)
            .unwrap()
            .unwrap()
            .as_str(),
        json["error"]["commit"]["head_after"].as_str().unwrap(),
        "the JSON must report the local checkpoint that actually landed"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "commit --push should attempt the push protocol once"
    );
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
fn log_json_regex_pickaxe_filters_and_reports_matched_paths() {
    let temp = fixture_repo();
    let dir = temp.path();
    // One commit whose diff adds the pattern in two files, one that adds it
    // in none.
    std::fs::write(dir.join("a.rs"), "fn handle_error_42() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn handle_retry_7() {}\n").unwrap();
    std::fs::write(dir.join("plain.txt"), "nothing to see\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("plain.txt"), "still nothing\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let json = json_stdout(&oak(dir, &["log", "--json", "-G", r"fn handle_\w+_\d+"]));
    let commits = json.as_array().expect("log JSON should be an array");
    assert_eq!(commits.len(), 1, "only the matching commit: {json}");
    let commit = &commits[0];
    assert_eq!(commit["files_changed"], 3);
    // The matched paths ride along so the reader doesn't pay a follow-up
    // diff per commit to find which files matched.
    assert_eq!(
        commit["pickaxe_matched_paths"]
            .as_array()
            .expect("matched paths listed")
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["a.rs", "b.rs"]
    );

    // Without a pickaxe the field is omitted entirely (append-only schema).
    let json = json_stdout(&oak(dir, &["log", "--json"]));
    let commits = json.as_array().expect("log JSON should be an array");
    assert!(commits
        .iter()
        .all(|commit| commit.get("pickaxe_matched_paths").is_none()));

    // A pickaxe with no matches is an empty array, not an error.
    let json = json_stdout(&oak(dir, &["log", "--json", "-G", "no_such_pattern"]));
    assert_eq!(json.as_array().expect("array").len(), 0);
}

#[test]
fn log_json_invalid_regex_is_a_json_error_envelope() {
    let temp = fixture_repo();

    let out = oak(temp.path(), &["log", "--json", "-G", "unclosed["]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "invalid regex is a usage error\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    // `--json` failures speak JSON on stdout — the error envelope, not a
    // bare stderr line an agent's parser never sees.
    let envelope: Value =
        serde_json::from_str(&stdout(&out)).expect("stdout should be a JSON error envelope");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("invalid regex") && message.contains("unclosed["),
        "envelope should carry the regex diagnostic: {message}"
    );
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
        serde_json::json!(["oak merge remote-feature"])
    );
    assert_eq!(current_branch(dir), original_branch);
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "base\n",
        "remote review must not rewrite the worktree"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn remote_branch_diff_hunks_do_not_use_local_checkout_bytes_when_blob_missing() {
    let temp = fixture_repo();
    let dir = temp.path();
    let main_head = seed_local_main_from_current_head(dir);
    let feature_head = seed_remote_feature_commit_with_missing_blob(dir, main_head.clone());
    std::fs::write(dir.join("tracked.txt"), "misleading local checkout\n").unwrap();

    let server = MockServer::start().await;
    mount_remote_branch_list(&server, &main_head, &feature_head).await;
    link_remote(dir, &server);

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
            "--hunks",
        ],
    ));

    assert_eq!(diff["kind"], "remote_branch_diff");
    assert_eq!(diff["hunks_truncated"], true);
    assert_eq!(diff["changed_files"][0]["patch_omitted"], true);
    assert!(diff["changed_files"][0].get("patch").is_none());
    assert!(
        !stdout(&oak(
            dir,
            &[
                "branch",
                "diff",
                "remote-feature",
                "--remote",
                "--against",
                "main",
                "--json",
                "--hunks",
            ],
        ))
        .contains("misleading local checkout"),
        "remote hunks must not fall back to unrelated local checkout bytes"
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

#[tokio::test(flavor = "current_thread")]
async fn close_local_branch_json_stays_single_document_after_sync_success() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);
    let server = MockServer::start().await;
    link_remote(dir, &server);
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

    let out = oak(dir, &["close", &branch, "--reason", "stale", "--json"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stderr(&out), "");
    assert_eq!(
        stdout(&out).lines().count(),
        1,
        "stdout must be exactly one JSON document"
    );
    let closed: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be valid JSON");
    assert_eq!(closed["branch"], branch);
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["close_reason"], "stale");
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
fn close_free_form_reason_is_preserved() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = current_branch(dir);
    assert!(oak(dir, &["switch", "-c", "feature-free-form-reason"])
        .status
        .success());
    assert!(oak(dir, &["switch", &original_branch, "--clean"])
        .status
        .success());

    let closed = json_stdout(&oak(
        dir,
        &[
            "close",
            "feature-free-form-reason",
            "--reason",
            "superseded by fb-46 cleanup",
            "--json",
        ],
    ));
    assert_eq!(closed["close_reason"], "superseded by fb-46 cleanup");

    let show = json_stdout(&oak(
        dir,
        &["branch", "show", "feature-free-form-reason", "--json"],
    ));
    assert_eq!(show["close_reason"], "superseded by fb-46 cleanup");
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
            && f.get("stats_available").is_none()
    }));
    assert!(files.iter().any(|f| {
        f["path"] == "new.txt"
            && f["status"] == "added"
            && f["additions"].is_number()
            && f.get("deletions").is_none()
            && f.get("stats_available").is_none()
    }));
    // Progressive disclosure: the structured hunk fetch is the first
    // recommendation, with the human-readable print as follow-up.
    assert_eq!(
        json["recommended_next_commands"][0],
        "oak diff --json --hunks"
    );
    assert!(json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command == "oak diff --print"));
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

    let truncated = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-json",
            "--against",
            "main",
            "--json",
            "--hunks",
            "--max-bytes",
            "1",
        ],
    ));
    assert_eq!(truncated["hunks_truncated"], true);
    assert_eq!(truncated["changed_files"][0]["patch_omitted"], true);
    assert_eq!(
        truncated["recommended_next_commands"][0],
        "oak branch diff feature-json --against main --json --hunks -- tracked.txt"
    );

    let filtered = json_stdout(&oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-json",
            "--against",
            "main",
            "--json",
            "--",
            "tracked.txt",
        ],
    ));
    assert_eq!(filtered["changed_file_count"], 1);
    assert_eq!(filtered["changed_files"][0]["path"], "tracked.txt");
    assert!(
        filtered.get("files_identical_to_against").is_none(),
        "path-filtered branch diffs must not emit a global identical-file summary"
    );

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
fn branch_diff_human_honors_path_filters() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-human-filter"])
        .status
        .success());
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("docs/keep.md"), "docs\n").unwrap();
    std::fs::write(dir.join("src/skip.rs"), "skip\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let out = oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-human-filter",
            "--against",
            "main",
            "--",
            "docs",
        ],
    );

    assert!(
        out.status.success(),
        "branch diff failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let stdout = stdout(&out);
    assert!(
        stdout.contains("docs/keep.md"),
        "filtered diff should include matching file:\n{stdout}"
    );
    assert!(
        !stdout.contains("src/skip.rs"),
        "filtered human diff must not show out-of-scope files:\n{stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_integrity_fetch_blocks_checkout_free_main_diff() {
    let temp = fixture_repo();
    let dir = temp.path();
    seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-fetch-failed"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "feature\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let server = MockServer::start().await;
    link_remote(dir, &server);
    let fake_head = "ab".repeat(32);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": fake_head
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": fake_head,
                "branch_name": "main",
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": oak_core::Tree::empty_hash().to_string(),
                "author": "<remote>",
                "message": "remote main",
                "timestamp": "2026-07-08T00:00:00Z",
                "files": []
            }],
            "trees": []
        })))
        .mount(&server)
        .await;

    let fetch = oak(dir, &["fetch"]);
    assert!(
        !fetch.status.success(),
        "fetch should fail integrity checks"
    );
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert!(repo
        .get_metadata(MetadataKey::MainLastFetchIntegrityError)
        .unwrap()
        .unwrap()
        .contains("does not reproduce its hash"));

    let diff = oak(
        dir,
        &[
            "branch",
            "diff",
            "feature-fetch-failed",
            "--against",
            "main",
            "--json",
        ],
    );
    assert!(!diff.status.success());
    let json: Value = serde_json::from_str(&stdout(&diff)).expect("stdout should be JSON");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("failed integrity verification"));

    let endpoint_diff = oak(
        dir,
        &[
            "diff",
            "feature-fetch-failed",
            "--against",
            "main",
            "--json",
        ],
    );
    assert!(!endpoint_diff.status.success());
    let json: Value = serde_json::from_str(&stdout(&endpoint_diff)).expect("stdout should be JSON");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("failed integrity verification"));
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
    assert!(limited["changed_files"][0]["additions"].is_number());
    assert!(limited["changed_files"][0].get("deletions").is_none());

    let limited_hunks = json_stdout(&oak(
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
            "--hunks",
            "--max-bytes",
            "10",
            "-U",
            "0",
        ],
    ));
    let next = limited_hunks["changed_files_page"]["next_page_command"]
        .as_str()
        .unwrap();
    assert!(next.contains("--hunks"), "{next}");
    assert!(next.contains("--max-bytes 10"), "{next}");
    assert!(next.contains("-U 0"), "{next}");

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-json-limit", "--json"],
    ));
    assert_eq!(review["changed_file_count"], 3);
    assert!(review["changed_files"][0]["additions"].is_number());
    assert!(review["changed_files"][0].get("deletions").is_none());
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
    // Fresh local main so the dry-run below is certified (exit 0) and the
    // test can focus on conflict reporting.
    mark_main_checked_now(dir);

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
fn actual_merge_json_errors_use_json_envelope() {
    let temp = fixture_repo();
    let dir = temp.path();

    let out = oak(dir, &["merge", "missing-branch", "--json"]);

    assert!(!out.status.success());
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["error"]["code"], "error");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("missing-branch"));
}

#[tokio::test]
async fn merge_force_json_posts_force_query_and_prints_success() {
    let temp = fixture_repo();
    let dir = temp.path();
    let branch_name = current_branch(dir);
    let branch_head = seed_local_main_from_current_head(dir);
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let manifest_hash = repo
        .get_commit(&branch_head)
        .unwrap()
        .unwrap()
        .manifest_hash;
    drop(repo);

    let server = MockServer::start().await;
    link_remote(dir, &server);

    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": branch_head.to_string()
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": branch_head.to_string()
        })))
        .expect(1)
        .mount(&server)
        .await;

    let squash_hash = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    Mock::given(method("POST"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}/merge")))
        .and(query_param("force", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": "forced squash merge",
            "commit_hash": squash_hash,
            "manifest_hash": manifest_hash.to_string(),
            "parent_hash": null,
            "merge_parent_hash": branch_head.to_string()
        })))
        .expect(1)
        .mount(&server)
        .await;

    let worktree = dir.to_path_buf();
    let out = tokio::task::spawn_blocking(move || oak(&worktree, &["merge", "--force", "--json"]))
        .await
        .unwrap();

    assert_eq!(stderr(&out), "");
    let stdout_text = stdout(&out);
    assert_eq!(
        stdout_text.lines().count(),
        1,
        "merge --force --json must print exactly one JSON document"
    );
    let json = json_stdout(&out);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["branch"], branch_name);
    assert_eq!(json["merged"], true);
    assert_eq!(json["parent"], "main");
    assert_eq!(json["message"], "forced squash merge");
    assert_eq!(json["commit_hash"], squash_hash);
    assert_eq!(json["manifest_hash"], manifest_hash.to_string());
    assert_eq!(json["merge_parent_hash"], branch_head.to_string());
    assert!(json["new_branch"].as_str().unwrap().starts_with("tester-"));
}

#[test]
fn branch_review_merge_preview_json_reports_clean_modify_modify_as_modified() {
    // fb-28: branch and main both modified the same file in disjoint
    // regions. The content merge is clean, so the preview must report the
    // file as modified with sane counts — not as a whole-file deletion just
    // because the path was absent from the predicted merged manifest.
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(dir.join("tracked.txt"), "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-disjoint"])
        .status
        .success());
    std::fs::write(
        dir.join("tracked.txt"),
        "BRANCH\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n",
    )
    .unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "l1\nl2\nl3\nl4\nl5\nl6\nl7\nMAIN\n");

    let review = json_stdout(&oak(
        dir,
        &[
            "branch",
            "review",
            "feature-disjoint",
            "--merge-preview",
            "--json",
        ],
    ));
    let preview = &review["merge_preview"];
    assert_eq!(preview["prediction_available"], true);
    assert_eq!(preview["clean"], true);
    assert_eq!(preview["conflict_file_count"], 0);
    // The embedded preview omits changed_files when identical to the review
    // summary; whichever list is authoritative must show a modification.
    let changed_files = if preview["changed_files"].is_array() {
        &preview["changed_files"]
    } else {
        &review["changed_files"]
    };
    let changed_files = changed_files.as_array().unwrap();
    assert_eq!(changed_files.len(), 1);
    let file = &changed_files[0];
    assert_eq!(file["path"], "tracked.txt");
    assert_eq!(
        file["status"], "modified",
        "clean modify/modify must not be misreported: {file}"
    );
    assert_eq!(file["additions"], 1);
    assert_eq!(file["deletions"], 1);
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
    // The predicted-conflict file (tracked.txt) is reported in
    // conflict_files only — never disguised as a changed (deleted) file —
    // so the preview counts just the two clean additions.
    assert_eq!(preview["changed_file_count"], 2);
    assert_eq!(preview["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(preview["changed_files_page"]["omitted_count"], 1);
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
    // A predicted conflict must never masquerade as a clean change (it
    // used to surface as a synthetic "deleted" row, because the conflicted
    // path is absent from the predicted merge manifest). It belongs in
    // conflict_files, and only there.
    assert!(
        !net_merge["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "tracked.txt"),
        "conflicted path must not appear as a clean change: {:?}",
        net_merge["changed_files"]
    );
    assert!(
        net_merge["conflict_files"]
            .as_array()
            .expect("net-merge diff names conflicts")
            .iter()
            .any(|path| path == "tracked.txt"),
        "got: {:?}",
        net_merge["conflict_files"]
    );
    assert_eq!(tree_tracked["status"], "modified");
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
    // Fresh local main so the explicit-branch dry-run is certified (exit 0).
    mark_main_checked_now(dir);

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
fn merge_dry_run_json_carries_four_tree_merge_safety_verdict() {
    // fb-105: the dry-run must classify the predicted result against the
    // four trees (fork base, branch head, current target head, predicted
    // result) and — because this local repo has never verified main against
    // the remote — must say so and refuse to certify.
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-safety"])
        .status
        .success());
    std::fs::write(dir.join("branch-only.txt"), "branch\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_adding_file(dir, base, "new_on_main.txt", "main\n");

    let out = oak(dir, &["merge", "--dry-run", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(7),
        "an uncertified dry-run must exit 7 so automation can tell it from certified-safe (0)\nstderr:\n{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("could not be certified") && stderr(&out).contains("oak fetch"),
        "stderr must state certification was not possible and how to fix it: {}",
        stderr(&out)
    );
    let dry_run: Value =
        serde_json::from_str(&stdout(&out)).expect("dry-run still prints its JSON before exiting");
    assert_eq!(dry_run["clean"], true);
    assert_eq!(dry_run["invariant_violations"], serde_json::json!([]));
    let safety = &dry_run["merge_safety"];
    assert_eq!(safety["certified"], false);
    assert_eq!(safety["verdict"], "uncertified");
    assert_eq!(safety["uncertified_cause"], "stale_local_target");
    assert_eq!(safety["target_head_source"], "stale_local");
    assert_eq!(safety["assessed_target"], "main");
    assert_eq!(safety["invariant_violation_count"], 0);
    assert!(
        safety["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason.as_str().unwrap().contains("refusing to certify")),
        "stale local target must be named explicitly: {safety}"
    );
    let branch_file = dry_run["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "branch-only.txt")
        .expect("branch contribution appears in changed_files");
    assert_eq!(branch_file["merge_safety"], "branch_change");
    assert!(
        dry_run["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["path"] != "new_on_main.txt"),
        "the target-only file must survive the predicted merge: {}",
        dry_run["changed_files"]
    );
    assert_eq!(
        dry_run["recommended_next_commands"],
        serde_json::json!(["oak fetch", "oak merge --dry-run --json"]),
        "an uncertified prediction must steer through a fetch before merging"
    );

    // The review surface exposes the same verdict inside merge_preview and
    // fails closed: no merge-safe claim, no merge recommendation.
    let review = json_stdout(&oak(
        dir,
        &[
            "branch",
            "review",
            "feature-safety",
            "--merge-preview",
            "--json",
        ],
    ));
    let preview = &review["merge_preview"];
    assert_eq!(preview["invariant_violations"], serde_json::json!([]));
    assert_eq!(preview["merge_safety"]["verdict"], "uncertified");
    assert_ne!(
        review["recommended_action"], "validate_then_merge",
        "an uncertified review must not recommend merging"
    );
    assert_eq!(review["vcs_merge_safe"], false);
    assert!(
        review["reason"]
            .as_str()
            .unwrap()
            .contains("merge_safety_uncertified"),
        "review reason must say certification was not possible: {}",
        review["reason"]
    );
    assert!(
        !review["recommended_next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("oak merge")),
        "uncertified review must steer to fetch, not merge: {}",
        review["recommended_next_commands"]
    );

    // Once local main is verified fresh, the same dry-run certifies and
    // exits 0.
    mark_main_checked_now(dir);
    let out = oak(dir, &["merge", "--dry-run", "--json"]);
    assert!(
        out.status.success(),
        "a certified, violation-free dry-run exits 0\nstderr:\n{}",
        stderr(&out)
    );
    let dry_run = json_stdout(&out);
    assert_eq!(dry_run["merge_safety"]["certified"], true);
    assert_eq!(dry_run["merge_safety"]["verdict"], "safe");
    assert_eq!(dry_run["merge_safety"]["target_head_source"], "fresh_local");
    assert_eq!(
        dry_run["recommended_next_commands"],
        serde_json::json!(["oak merge"])
    );
}

#[test]
fn merge_dry_run_json_exits_uncertified_when_no_prediction_ran_at_all() {
    // fb-105 fail-closed hole: "no prediction ran" is the WEAKEST evidence
    // there is, and it used to exit 0 — the same signal as "certified
    // safe". A fresh repo with no local main head is the ordinary
    // partially-fetched shape: no parent head => no three-way prediction =>
    // no four-tree classification. The exit code must say "unable to
    // certify" (7), not "safe" (0), without anyone parsing JSON.
    let temp = fixture_repo();
    let dir = temp.path();
    let branch = current_branch(dir);

    let out = oak(dir, &["merge", "--dry-run", "--json"]);

    assert_eq!(
        out.status.code(),
        Some(7),
        "a dry-run that never produced a prediction must exit 7, not 0\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("prediction_unavailable")
            && stderr(&out).contains("could not be certified")
            && stderr(&out).contains("oak fetch"),
        "stderr must name the cause and the fix: {}",
        stderr(&out)
    );

    // stdout is still exactly one JSON document — the payload, printed
    // before the non-zero exit, with no error envelope appended.
    let dry_run: Value = serde_json::from_str(&stdout(&out))
        .expect("dry-run prints its JSON payload before exiting non-zero");
    assert_eq!(dry_run["prediction_available"], false);
    assert_eq!(dry_run["clean"], Value::Null);
    assert!(
        dry_run.get("error").is_none(),
        "the verdict payload must not be followed by an error envelope: {}",
        stdout(&out)
    );
    assert!(
        dry_run["merge_safety"].is_null(),
        "no classification ran, so there is no merge_safety block: {dry_run}"
    );
    assert!(
        dry_run["invariant_violations"].is_null(),
        "absent violations mean 'not computed', never 'checked and clean': {dry_run}"
    );

    // The other end of the steering chain: review must not hand an agent a
    // merge-executing command (nor claim merge safety) for this shape.
    let review = json_stdout(&oak(
        dir,
        &["branch", "review", &branch, "--merge-preview", "--json"],
    ));
    assert_eq!(review["merge_preview"]["prediction_available"], false);
    assert_ne!(review["recommended_action"], "validate_then_merge");
    assert_ne!(review["vcs_merge_safe"], true);
    let recommended = review["recommended_next_commands"]
        .as_array()
        .expect("review recommends next commands")
        .iter()
        .map(|command| command.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        recommended
            .iter()
            .all(|command| !is_merge_executing_command(command)),
        "an unassessable branch must never be steered into a merge: {recommended:?}"
    );
}

/// A command that actually performs a merge. `oak merge --dry-run --json`
/// is explicitly NOT one: it is exactly where an uncertified verdict should
/// steer an agent back to.
fn is_merge_executing_command(command: &str) -> bool {
    let trimmed = command.trim();
    (trimmed == "oak merge" || trimmed.starts_with("oak merge ")) && !trimmed.contains("--dry-run")
}

#[test]
fn merge_dry_run_json_emits_error_envelope_when_it_fails_before_its_payload() {
    // The JSON contract is "a --json command always emits a JSON document".
    // Suppressing the error envelope for the whole dry-run (so the verdict
    // payload stays the single document on stdout) also dropped it for
    // failures that happen BEFORE any payload — those exited with zero
    // bytes on stdout. Suppression must be scoped to "a payload was already
    // printed".
    let outside = tempfile::TempDir::new().unwrap();
    let dir = outside.path();

    let out = oak(dir, &["merge", "--dry-run", "--json"]);
    assert_eq!(out.status.code(), Some(1), "stderr:\n{}", stderr(&out));
    let envelope: Value = serde_json::from_str(&stdout(&out)).unwrap_or_else(|err| {
        panic!(
            "a pre-payload dry-run failure must still emit a JSON error envelope \
             (got {} bytes on stdout: {:?}; {err})",
            stdout(&out).len(),
            stdout(&out)
        )
    });
    assert_eq!(envelope["schema_version"], 1);
    assert_eq!(envelope["error"]["code"], "repo_not_found");

    // Same shape as the non-dry-run and non-merge JSON commands.
    let plain_merge: Value = serde_json::from_str(&stdout(&oak(dir, &["merge", "--json"])))
        .expect("oak merge --json emits an envelope outside a repo");
    let status: Value = serde_json::from_str(&stdout(&oak(dir, &["status", "--json"])))
        .expect("oak status --json emits an envelope outside a repo");
    assert_eq!(envelope["error"]["code"], plain_merge["error"]["code"]);
    assert_eq!(envelope["error"]["code"], status["error"]["code"]);

    // A rejected flag combination is also pre-payload.
    let repo = fixture_repo();
    let out = oak(repo.path(), &["merge", "--dry-run", "--json", "--force"]);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", stderr(&out));
    let envelope: Value = serde_json::from_str(&stdout(&out)).unwrap_or_else(|err| {
        panic!(
            "a rejected dry-run flag combination must still emit an envelope \
             (stdout: {:?}; {err})",
            stdout(&out)
        )
    });
    assert_eq!(envelope["error"]["code"], "invalid_argument");
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
    assert_eq!(review["recommended_action_detail"]["kind"], "close_branch");
    assert_eq!(
        review["recommended_action_detail"]["command"],
        "oak close empty-branch --reason stale --json"
    );
    assert_eq!(review["recommended_action_detail"]["mutates"], true);
    assert_eq!(review["recommended_action_detail"]["needs_network"], false);
    assert_eq!(review["recommended_action_detail"]["confidence"], "high");
    assert_eq!(
        review["recommended_action_detail"]["remote_freshness"],
        "not_configured"
    );
    assert_eq!(review["reason"], "empty");
    assert_eq!(review["close_allowed"], true);
    assert_eq!(review["contribution"], "empty");
    assert_eq!(review["merge_allowed"], false);
    assert_eq!(review["checks"]["required"], true);
    assert_eq!(review["checks"]["known_passed"], false);
    assert_eq!(review["checks"]["source"], Value::Null);
    assert_eq!(review["vcs_merge_safe"], Value::Null);
    assert_eq!(
        review["recommended_next_commands"],
        serde_json::json!(["oak close empty-branch --reason stale --json"])
    );
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
    assert_eq!(
        review["recommended_next_commands"],
        serde_json::json!(["oak close feature-superseded --reason stale --json"])
    );
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
    mark_main_checked_now(dir);

    let review = json_stdout(&oak(dir, &["branch", "review", "feature-clean", "--json"]));

    assert_eq!(review["recommended_action"], "validate_then_merge");
    assert_eq!(review["recommended_action_detail"]["kind"], "merge");
    assert_eq!(
        review["recommended_action_detail"]["command"],
        "oak merge feature-clean"
    );
    assert_eq!(review["recommended_action_detail"]["mutates"], true);
    assert_eq!(review["recommended_action_detail"]["needs_network"], true);
    assert_eq!(review["recommended_action_detail"]["confidence"], "medium");
    assert_eq!(review["reason"], "clean_contribution");
    assert_eq!(review["close_allowed"], false);
    assert_eq!(review["contribution"], "contributes");
    assert_eq!(review["mergeability"], "clean");
    assert_eq!(review["vcs_merge_safe"], true);
    assert_eq!(review["merge_allowed"], false);
    assert_eq!(review["target_risk"], "none");
}

#[test]
fn branch_review_triage_marker_conflict_stays_review_when_target_risk_unknown() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-conflict-triage"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");
    mark_main_checked_now(dir);

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
fn branch_triage_marker_conflict_stays_review_when_target_risk_unknown() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-conflict-batch"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(dir, base, "main\n");
    mark_main_checked_now(dir);

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
fn branch_review_triage_add_add_marker_conflict_stays_review_when_target_risk_unknown() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-additive-conflict"])
        .status
        .success());
    std::fs::write(dir.join("shared.txt"), "branch addition\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_adding_file(dir, base, "shared.txt", "main addition\n");
    mark_main_checked_now(dir);

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-additive-conflict", "--json"],
    ));

    assert_eq!(review["recommended_action"], "review");
    assert_eq!(review["reason"], "target_risk_unknown");
    assert_eq!(review["mergeability"], "conflicts");
    assert_eq!(review["target_risk"], "unknown");
    assert_eq!(review["vcs_merge_safe"], false);
    assert_ne!(review["recommended_action"], "validate_then_merge");
    assert_ne!(review["recommended_action"], "resolve");
}

#[test]
fn branch_review_triage_marker_free_same_file_merge_keeps_target_risk_none() {
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(
        dir.join("tracked.txt"),
        "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n",
    )
    .unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-marker-free"])
        .status
        .success());
    std::fs::write(
        dir.join("tracked.txt"),
        "BRANCH\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n",
    )
    .unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_with_tracked_txt(
        dir,
        base,
        "line1\nline2\nline3\nline4\nline5\nline6\nline7\nTARGET\n",
    );
    mark_main_checked_now(dir);

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-marker-free", "--json"],
    ));

    assert_eq!(review["recommended_action"], "validate_then_merge");
    assert_eq!(review["reason"], "clean_contribution");
    assert_eq!(review["mergeability"], "clean");
    assert_eq!(review["target_risk"], "none");
    assert_eq!(review["vcs_merge_safe"], true);
}

#[test]
fn branch_review_triage_branch_wins_target_delete_rebuilds() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-reverts-target"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch keeps stale file\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_deleting_tracked_txt(dir, base);

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-reverts-target", "--json"],
    ));

    assert_eq!(review["recommended_action"], "rebuild");
    assert_eq!(review["reason"], "reverts_target_exact");
    assert_eq!(review["contribution"], "contributes");
    assert_eq!(review["mergeability"], "conflicts");
    assert_eq!(review["target_risk"], "reverts_target_exact");
    assert_eq!(
        review["recommended_next_commands"],
        serde_json::json!(["oak branch review feature-reverts-target --merge-preview --json"])
    );
    assert_ne!(review["recommended_action"], "validate_then_merge");
    assert_ne!(review["recommended_action"], "resolve");
    assert!(
        !review["recommended_next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("oak merge")),
        "target-delete rebuild must not recommend merge commands: {:?}",
        review["recommended_next_commands"]
    );
}

#[test]
fn branch_triage_branch_wins_target_delete_matches_single_review() {
    let temp = fixture_repo();
    let dir = temp.path();
    let base = seed_local_main_from_current_head(dir);
    assert!(oak(dir, &["switch", "-c", "feature-reverts-target-batch"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch keeps stale file\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    advance_main_deleting_tracked_txt(dir, base);

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
        .find(|row| row["branch"] == "feature-reverts-target-batch")
        .expect("feature-reverts-target-batch row");

    let review = json_stdout(&oak(
        dir,
        &["branch", "review", "feature-reverts-target-batch", "--json"],
    ));

    assert_eq!(row["recommended_action"], "rebuild");
    assert_eq!(row["reason"], "reverts_target_exact");
    assert_eq!(row["target_risk"], "reverts_target_exact");
    assert_eq!(row["recommended_action"], review["recommended_action"]);
    assert_eq!(row["reason"], review["reason"]);
    assert_eq!(row["target_risk"], review["target_risk"]);
    assert_ne!(row["recommended_action"], "validate_then_merge");
    assert_ne!(row["recommended_action"], "resolve");
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
    assert_eq!(review["recommended_action_detail"]["kind"], "merge");
    assert_eq!(
        review["recommended_action_detail"]["command"],
        "oak merge remote-feature"
    );
    assert_eq!(review["recommended_action_detail"]["mutates"], true);
    assert_eq!(review["recommended_action_detail"]["needs_network"], true);
    assert_eq!(
        review["recommended_action_detail"]["remote_freshness"],
        "fresh"
    );
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

fn seed_local_scale_triage_branches(dir: &Path, count: usize) -> String {
    let main_head = seed_local_main_from_current_head(dir);
    let original_branch = current_branch(dir);
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let parent_commit = repo.get_commit(&main_head).unwrap().unwrap();
    let parent_manifest = repo
        .get_manifest(&parent_commit.manifest_hash)
        .unwrap()
        .unwrap();

    for index in 0..count {
        let branch = format!("scale-feature-{index:04}");
        let path = format!("scale/{index:04}.txt");
        let content = format!("scale branch {index}\n");
        let blob = repo.put_blob(content.into_bytes()).unwrap();
        let mut entries = parent_manifest.entries.clone();
        entries.push(ManifestEntry {
            path: path.clone(),
            blob_hash: blob.clone(),
            mode: FileMode::Regular,
        });
        let manifest = Manifest::new(entries);
        repo.store_manifest(&manifest).unwrap();
        let commit = Commit::new(
            branch.clone(),
            Some(main_head.clone()),
            None,
            manifest.hash.clone(),
            "tester".to_string(),
            Some(format!("scale branch {index}")),
            vec![FileChange {
                path,
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
        repo.store_branch(&Branch::new(
            branch.clone(),
            Some(format!("scale branch {index}")),
            Some("main".to_string()),
        ))
        .unwrap();
        repo.store_commit(&commit).unwrap();
        repo.set_branch_head(&branch, &commit.hash).unwrap();
    }
    // Fresh local main so merge-safety certifies and the scale rows keep
    // their validate-then-merge recommendations (fb-105 fail-closed).
    mark_main_checked_now(dir);

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
fn branch_triage_json_n10_scale_gate() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = seed_local_scale_triage_branches(dir, 10);

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
    let rows: Vec<&Value> = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row["branch"]
                .as_str()
                .unwrap()
                .starts_with("scale-feature-")
        })
        .collect();

    assert_eq!(rows.len(), 10);
    assert!(rows
        .iter()
        .all(|row| row["recommended_action_detail"]["kind"] == "merge"));
    assert_eq!(current_branch(dir), original_branch);
}

#[test]
fn branch_triage_json_n100_scale_gate() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = seed_local_scale_triage_branches(dir, 100);

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
    let rows: Vec<&Value> = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row["branch"]
                .as_str()
                .unwrap()
                .starts_with("scale-feature-")
        })
        .collect();

    assert_eq!(rows.len(), 100);
    assert!(rows
        .iter()
        .all(|row| row["recommended_action_detail"]["kind"] == "merge"));
    assert_eq!(current_branch(dir), original_branch);
}

#[test]
#[ignore = "stress gate for launch validation; run explicitly before branch-review claims"]
fn branch_triage_json_n1000_stress_gate() {
    let temp = fixture_repo();
    let dir = temp.path();
    let original_branch = seed_local_scale_triage_branches(dir, 1000);

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
    let rows: Vec<&Value> = triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row["branch"]
                .as_str()
                .unwrap()
                .starts_with("scale-feature-")
        })
        .collect();

    assert_eq!(rows.len(), 1000);
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
    assert!(rows
        .iter()
        .filter(|row| {
            row["missing_data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "analysis_deferred_by_limit")
        })
        .all(|row| row["analysis_budget_exhausted"].as_bool() == Some(true)));
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

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "remote batch triage should fetch the branch list once instead of once per branch"
    );
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
    assert_eq!(
        batch_row["recommended_action_detail"],
        review["recommended_action_detail"]
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
    assert_eq!(row["recommended_action_detail"]["kind"], "close_branch");
    assert_eq!(
        row["recommended_action_detail"]["command"],
        "oak close hunk-superseded --reason stale --json"
    );
    assert_eq!(row["recommended_action_detail"]["mutates"], true);
    assert_eq!(row["recommended_action_detail"]["needs_network"], false);
    assert_eq!(row["reason"], "superseded_exact");
    assert_eq!(row["close_allowed"], true);
    assert_eq!(row["contribution"], "superseded_exact");
    assert_eq!(row["mergeability"], "conflicts");
    assert_eq!(row["target_risk"], "unknown");
    assert_ne!(row["recommended_action"], "validate_then_merge");
    assert_ne!(row["recommended_action"], "resolve");
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
    assert_eq!(review["mergeability"], "conflicts");
    assert_eq!(review["target_risk"], "unknown");
    assert_ne!(review["recommended_action"], "validate_then_merge");
    assert_ne!(review["recommended_action"], "resolve");
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
