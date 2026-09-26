//! Integration tests for Oak space scaffolding and agent-facing contracts.

use std::path::Path;
use std::process::{Command, Output};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("OAK_MOUNTS_ROOT", dir.join("absent-test-mount-registry"))
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

#[test]
fn inventory_observes_checkout_without_claiming_cleanup_safety() {
    use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
    let root = tempfile::tempdir().unwrap();
    let repo_root = root.path().join("task/repo");
    std::fs::create_dir_all(repo_root.join(".oak")).unwrap();
    let path = repo_root.join(".oak/oak.db");
    let repo = SqliteRepository::open(&path).unwrap();
    repo.store_branch(&Branch::new("task".into(), None, Some("main".into())))
        .unwrap();
    repo.set_current_branch("task").unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
    drop(repo);
    let before = std::fs::read(&path).unwrap();
    let out = oak(root.path(), &["space", "inventory", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let entry = &value["entries"][0];
    assert_eq!(entry["branch"], "task");
    assert_eq!(entry["repo_owner"], "acme");
    assert_eq!(entry["working_tree"], "unverified");
    assert_eq!(entry["remote_publication"], "unverified");
    assert!(entry.get("safe_to_delete").is_none());
    assert_eq!(before, std::fs::read(&path).unwrap());
}

#[test]
fn inventory_budget_does_not_silently_drop_discovered_directories() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("one")).unwrap();
    let out = oak(
        root.path(),
        &["space", "inventory", "--max-entries", "2", "--json"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["complete"], false);
    assert!(!value["coverage_gaps"].as_array().unwrap().is_empty());
    assert_eq!(value["examined_entries"], 2);
}

#[test]
fn inventory_observes_registered_mount_without_walking_its_contents() {
    use oak_cli::commands::mount::state::MountConfig;
    let root = tempfile::tempdir().unwrap();
    let mounts = tempfile::tempdir().unwrap();
    let dest = std::fs::canonicalize(root.path()).unwrap().join("lazy");
    std::fs::create_dir_all(dest.join("must-not-traverse/.oak")).unwrap();
    let state = mounts.path().join("mount1");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("daemon.pid"), std::process::id().to_string()).unwrap();
    let cfg = MountConfig {
        id: "mount1".into(),
        mount_point: dest.clone(),
        remote_url: "https://secret@private.example".into(),
        owner: "acme".into(),
        repo: "lazy".into(),
        base_branch: "main".into(),
        base_commit: "a".repeat(64),
        virtual_branch: "task-mount".into(),
        mounted_branch: None,
    };
    std::fs::write(state.join("config.toml"), toml::to_string(&cfg).unwrap()).unwrap();
    std::fs::write(
        mounts.path().join("index.json"),
        serde_json::json!({"mounts":{dest.to_str().unwrap():"mount1"}}).to_string(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["space", "inventory", "--json"])
        .current_dir(root.path())
        .env("OAK_MOUNTS_ROOT", mounts.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    let item = &value["entries"][0];
    assert_eq!(item["kind"], "mount");
    assert_eq!(item["branch"], "task-mount");
    assert_eq!(item["daemon_pid_alive"], true);
    assert_eq!(item["working_tree"], "unverified");
    assert!(!stdout(&out).contains("secret"));
    assert!(dest.join("must-not-traverse/.oak").exists());
}

#[test]
fn readonly_inspection_preserves_known_older_schema_and_wal_snapshot() {
    use oak_core::{MetadataKey, Repository, SqliteRepository};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.db");
    let writer = SqliteRepository::open(&path).unwrap();
    writer
        .set_metadata(MetadataKey::RepoName, "before")
        .unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute("DELETE FROM schema_migrations WHERE version = (SELECT MAX(version) FROM schema_migrations)",[]).unwrap();
    let reader = SqliteRepository::open_read_only(&path).unwrap();
    assert_eq!(
        reader
            .get_metadata(MetadataKey::RepoName)
            .unwrap()
            .as_deref(),
        Some("before")
    );
    writer.set_metadata(MetadataKey::RepoName, "after").unwrap();
    assert_eq!(
        reader
            .get_metadata(MetadataKey::RepoName)
            .unwrap()
            .as_deref(),
        Some("before")
    );
    assert!(reader
        .set_metadata(MetadataKey::RepoName, "forbidden")
        .is_err());
    assert_eq!(
        writer
            .get_metadata(MetadataKey::RepoName)
            .unwrap()
            .as_deref(),
        Some("after")
    );
    assert!(SqliteRepository::open_read_only(&temp.path().join("missing.db")).is_err());
    assert!(!temp.path().join("missing.db").exists());
}

#[test]
fn inventory_root_inside_registered_mount_does_not_traverse_or_escape_scope() {
    let root = tempfile::tempdir().unwrap();
    let mounts = tempfile::tempdir().unwrap();
    let mount = std::fs::canonicalize(root.path()).unwrap().join("lazy");
    let inside = mount.join("inside");
    std::fs::create_dir_all(inside.join("accidental/.oak")).unwrap();
    std::fs::write(
        mounts.path().join("index.json"),
        serde_json::json!({"mounts":{mount.to_str().unwrap():"mount1"}}).to_string(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["space", "inventory", "--json"])
        .current_dir(&inside)
        .env("OAK_MOUNTS_ROOT", mounts.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(value["entries"].as_array().unwrap().is_empty());
    assert_eq!(value["complete"], false);
}

#[test]
fn inventory_does_not_descend_into_git_backed_repository_internals() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("repo/.git/accidental/.oak")).unwrap();
    let out = oak(root.path(), &["space", "inventory", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    assert_eq!(value["entries"][0]["kind"], "git_checkout");
    assert_eq!(value["entries"][0]["observation"], "unavailable");
}

#[test]
fn inventory_unreadable_and_interrupted_checkouts_remain_unavailable() {
    let root = tempfile::tempdir().unwrap();
    for name in ["missing", "corrupt", "interrupted"] {
        std::fs::create_dir_all(root.path().join(name).join(".oak")).unwrap();
    }
    std::fs::write(root.path().join("corrupt/.oak/oak.db"), b"not sqlite").unwrap();
    std::fs::write(
        root.path().join("interrupted/.oak/CLONE_IN_PROGRESS"),
        b"pending",
    )
    .unwrap();
    let out = oak(root.path(), &["space", "inventory", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["entries"].as_array().unwrap().len(), 3);
    for entry in value["entries"].as_array().unwrap() {
        assert_eq!(entry["observation"], "unavailable");
        assert_eq!(entry["working_tree"], "unverified");
    }
    assert!(!root.path().join("missing/.oak/oak.db").exists());
    assert_eq!(
        std::fs::read(root.path().join("corrupt/.oak/oak.db")).unwrap(),
        b"not sqlite"
    );
    assert_eq!(
        value["entries"][1]["progress_markers"][0],
        "CLONE_IN_PROGRESS"
    );
}

#[cfg(unix)]
#[test]
fn inventory_does_not_follow_symlinks_or_resolve_an_ancestor_repository() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join(".oak")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    let out = oak(root.path(), &["space", "inventory", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(value["entries"].as_array().unwrap().is_empty());
    assert_eq!(value["complete"], false);
    std::fs::create_dir(root.path().join(".oak")).unwrap();
    std::fs::create_dir(root.path().join("inside")).unwrap();
    let out = oak(
        &root.path().join("inside"),
        &["space", "inventory", "--json"],
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(value["entries"].as_array().unwrap().is_empty());
}

#[test]
fn space_repos_without_org_or_marker_is_usage_error() {
    let temp = tempfile::TempDir::new().unwrap();

    let out = oak(
        temp.path(),
        &["space", "repos", "--remote", "http://127.0.0.1:9"],
    );

    assert_eq!(
        out.status.code(),
        Some(2),
        "expected usage exit code\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("no org given and no .oak-space marker found here"),
        "stderr should explain the missing space marker:\n{}",
        stderr(&out)
    );
}

#[test]
fn space_new_scaffolds_org_space_with_finish_references() {
    let temp = tempfile::TempDir::new().unwrap();
    let space = temp.path().join("acme-space");

    let out = oak(
        temp.path(),
        &[
            "space",
            "new",
            "acme/blog",
            space.to_str().unwrap(),
            "--remote",
            "http://127.0.0.1:9",
        ],
    );

    assert!(
        out.status.success(),
        "space new should scaffold despite repo-list warning\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );

    assert_eq!(
        std::fs::read_to_string(space.join(".oak-space")).unwrap(),
        "acme\n"
    );
    assert!(space.join("CLAUDE.md").exists());
    assert!(space.join(".claude/settings.json").exists());

    let agents = std::fs::read_to_string(space.join("AGENTS.md")).unwrap();
    assert!(
        agents.contains("oak finish --desc-file \"$DESC_FILE\""),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak mount finish [path] --desc-file <file>"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak desc --file <file>"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak mount list --json"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak agent state --json"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak agent state --json --compact"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak status --porcelain"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak status --short"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak diff --name-only"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak log --oneline -n N"),
        "AGENTS.md:\n{agents}"
    );

    let settings = std::fs::read_to_string(space.join(".claude/settings.json")).unwrap();
    assert!(settings.contains("Bash(oak finish:*)"));
}
