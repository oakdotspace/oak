//! End-to-end-ish tests for the mount lifecycle that don't need FUSE itself.
//!
//! We construct a mount state directory by hand (the same shape `start()`
//! produces), simulate an overlay write, then call `commit` / `status` /
//! `forget` and verify the side-effects.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use oak_cli::commands::mount;
use oak_cli::output;
use oak_core::{Branch, BranchStatus, Hash, ManifestEntry, MetadataKey};
use oak_core::{Repository, SqliteRepository};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const AGENT_STATE_SCHEMA_VERSION: i64 = 2;

/// Give the *current test thread* its own isolated mounts root, so each test
/// stores state under a private temp dir rather than the user's
/// `~/.oak/mounts/` — and, crucially, never shares the mount index with any
/// other test. `cargo test` runs each test on its own thread, so the
/// per-thread override in [`mount::state::set_mounts_root`] makes the roots
/// fully disjoint and eliminates the parallel index race. The temp dir is
/// leaked so it stays alive for the whole test (commands re-resolve the root
/// on every call, so we never need to hand the handle back to the caller).
fn isolated_root() {
    let temp = TempDir::new().expect("temp dir for mounts root");
    mount::state::set_mounts_root(temp.path().to_path_buf());
    std::mem::forget(temp); // keep the dir alive for the test's duration
}

fn oak_with_mounts_root(dir: &Path, args: &[&str], mounts_root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("OAK_MOUNTS_ROOT", mounts_root)
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

/// Build a mount state dir for `dest`, with one base file `README.md` whose
/// blob is pre-cached. Returns the state-dir path.
fn build_mount(dest: &Path) -> std::path::PathBuf {
    build_mount_with_remote(dest, "https://oak.example")
}

fn build_mount_with_remote(dest: &Path, remote_url: &str) -> std::path::PathBuf {
    build_mount_with_remote_and_parent_auth(dest, remote_url, None, Some("test-token"))
}

fn build_mount_with_remote_without_auth(dest: &Path, remote_url: &str) -> std::path::PathBuf {
    build_mount_with_remote_and_parent_auth(dest, remote_url, None, None)
}

fn build_mount_with_remote_and_parent(
    dest: &Path,
    remote_url: &str,
    base_parent: Option<Hash>,
) -> std::path::PathBuf {
    build_mount_with_remote_and_parent_auth(dest, remote_url, base_parent, Some("test-token"))
}

fn build_mount_with_remote_and_parent_auth(
    dest: &Path,
    remote_url: &str,
    base_parent: Option<Hash>,
    api_key: Option<&str>,
) -> std::path::PathBuf {
    isolated_root();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let state_dir = mount::state::state_dir_for(&id).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(mount::state::overlay_dir(&state_dir)).unwrap();

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    cache
        .set_metadata(MetadataKey::RemoteUrl, remote_url)
        .unwrap();
    cache.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    cache.set_metadata(MetadataKey::RepoName, "myrepo").unwrap();
    if let Some(api_key) = api_key {
        cache.set_metadata(MetadataKey::ApiKey, api_key).unwrap();
    }

    let base = Branch {
        name: "main".to_string(),
        description: None,
        parent_branch: None,
        status: BranchStatus::Open,
        close_reason: None,
        created_at: chrono::Utc::now(),
    };
    cache.store_branch(&base).unwrap();

    // Base file: store its blob, build a manifest containing it.
    let readme_content = b"# hello\n".to_vec();
    let readme_blob = oak_core::Blob::new(readme_content.clone());
    let readme_hash = readme_blob.hash.clone();
    cache.store_blob(&readme_blob).unwrap();
    let manifest_hash = cache
        .put_manifest(vec![ManifestEntry {
            path: "README.md".into(),
            blob_hash: readme_hash,
            mode: oak_core::FileMode::Regular,
        }])
        .unwrap();

    let base_commit_hash = cache
        .put_commit(
            "main".into(),
            base_parent,
            None,
            manifest_hash,
            "tester".into(),
            Some("initial".into()),
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    cache.set_branch_head("main", &base_commit_hash).unwrap();

    let virtual_branch = format!("main--mount-{}", &id[..8]);
    let v_branch = Branch::new(
        virtual_branch.clone(),
        Some("test mount".into()),
        Some("main".into()),
    );
    cache.store_branch(&v_branch).unwrap();
    cache
        .set_branch_head(&virtual_branch, &base_commit_hash)
        .unwrap();
    cache.set_current_branch(&virtual_branch).unwrap();
    cache.set_head(&base_commit_hash).unwrap();

    let cfg = mount::state::MountConfig {
        id: id.clone(),
        mount_point: dest.to_path_buf(),
        remote_url: remote_url.into(),
        owner: "oak".into(),
        repo: "myrepo".into(),
        base_branch: "main".into(),
        base_commit: base_commit_hash.as_str().to_string(),
        virtual_branch,
        mounted_branch: None,
    };
    mount::state::save_config(&state_dir, &cfg).unwrap();
    mount::state::register_mount(dest, &id).unwrap();

    state_dir
}

#[test]
fn commit_picks_up_dirty_overlay_file() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    // Write a dirty file to the overlay representing a user edit of "README.md".
    let new_content = b"# hello, mount!\n".to_vec();
    let overlay_file = mount::state::overlay_filename_for("README.md");
    fs::write(
        mount::state::overlay_dir(&state_dir).join(&overlay_file),
        &new_content,
    )
    .unwrap();
    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.dirty.insert(
        "README.md".into(),
        mount::state::DirtyEntry {
            overlay_file,
            mode: "regular".into(),
            in_place: false,
        },
    );
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    mount::status(dest).expect("status should succeed");
    mount::commit(dest).expect("commit should succeed");

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let new_head = cache
        .get_branch_head(&cfg.virtual_branch)
        .unwrap()
        .expect("virtual branch has head");
    assert_ne!(
        new_head.as_str(),
        cfg.base_commit,
        "virtual branch head should advance past base commit"
    );

    let new_commit = cache.get_commit(&new_head).unwrap().expect("commit stored");
    // Mount commits land on a virtual feature branch, so they don't carry
    // a message under the new model — branch descriptions are the source
    // of truth and only the server's squash-merge to main writes one.
    assert_eq!(new_commit.message, None);
    assert_eq!(new_commit.branch_name, cfg.virtual_branch);
    assert_eq!(
        new_commit.parent_hash.as_ref().map(|h| h.as_str()),
        Some(cfg.base_commit.as_str())
    );

    let manifest = cache
        .get_manifest(&new_commit.manifest_hash)
        .unwrap()
        .expect("manifest stored");
    assert_eq!(manifest.entries.len(), 1);
    assert_eq!(manifest.entries[0].path, "README.md");
    let new_blob_hash = &manifest.entries[0].blob_hash;
    let stored = cache
        .get_blob(new_blob_hash)
        .unwrap()
        .expect("new blob stored in cache");
    assert_eq!(stored.content, new_content);

    let post_overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    assert!(post_overlay.dirty.is_empty());
    assert!(post_overlay.deletions.is_empty());
    assert!(post_overlay.renames.is_empty());
    assert!(
        fs::read_dir(mount::state::overlay_dir(&state_dir))
            .unwrap()
            .next()
            .is_none(),
        "overlay dir should be empty after commit"
    );
}

#[test]
fn commit_handles_deletion() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.deletions.push("README.md".into());
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    mount::commit(dest).expect("commit should succeed");

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let new_head = cache.get_branch_head(&cfg.virtual_branch).unwrap().unwrap();
    let new_commit = cache.get_commit(&new_head).unwrap().unwrap();
    let manifest = cache
        .get_manifest(&new_commit.manifest_hash)
        .unwrap()
        .unwrap();
    assert!(manifest.entries.is_empty(), "deleted file should be gone");
}

#[test]
fn commit_no_changes_is_noop() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let head_before = cache.get_branch_head(&cfg.virtual_branch).unwrap();

    mount::commit(dest).expect("commit on clean tree should succeed");

    let head_after = cache.get_branch_head(&cfg.virtual_branch).unwrap();
    assert_eq!(head_before, head_after, "branch head should not move");
}

#[test]
fn forget_refuses_when_dirty() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.deletions.push("README.md".into());
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    let err = mount::end(dest, false).expect_err("end should reject dirty mount");
    let msg = err.to_string();
    assert!(
        msg.contains("uncommitted") || msg.contains("dirty"),
        "should mention dirty state: {msg}"
    );
    assert!(state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
}

#[test]
fn forget_clears_clean_mount() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    mount::end(dest, false).expect("end on clean mount should succeed");
    assert!(!state_dir.exists(), "state dir should be removed");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[test]
fn mount_dest_for_finds_registered_mount() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let _state_dir = build_mount(dest);

    // The mount-point itself resolves to itself.
    let resolved = mount::mount_dest_for(dest).unwrap();
    let canonical = std::fs::canonicalize(dest).unwrap();
    assert_eq!(resolved, Some(canonical.clone()));

    // A nested subdirectory inside the mount also resolves to the
    // mount-point, mirroring how `oak commit` works from any subdir of a
    // regular repo.
    let sub = dest.join("nested/dir");
    std::fs::create_dir_all(&sub).unwrap();
    let resolved_sub = mount::mount_dest_for(&sub).unwrap();
    assert_eq!(resolved_sub, Some(canonical));
}

#[test]
fn mount_dest_for_returns_none_outside_mount() {
    // Isolate the index so we don't read the real `~/.oak/mounts/index.json`.
    isolated_root();
    // A fresh temp dir that's never been registered shouldn't resolve.
    let temp = TempDir::new().unwrap();
    let resolved = mount::mount_dest_for(temp.path()).unwrap();
    assert!(resolved.is_none());
}

#[test]
fn log_and_diff_succeed_when_clean() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let _state_dir = build_mount(dest);

    // log/diff should succeed on a clean mount; we don't capture stdout here,
    // but the surface contract is "doesn't error". They exercise the same
    // cache-open path. `print = true` keeps diff on its plain-text path rather
    // than trying to open the interactive browser.
    mount::log(dest, Some(10), false).expect("log on a clean mount should succeed");
    mount::diff(dest, true, &[], false, false).expect("diff on a clean mount should succeed");
}

#[test]
fn log_walks_from_virtual_head_even_when_cached_commit_is_on_main() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let missing_parent = Hash::from_hex(&"1".repeat(64)).unwrap();
    let _state_dir =
        build_mount_with_remote_and_parent(dest, "https://oak.example", Some(missing_parent));

    output::begin_capture();
    mount::log(dest, Some(10), false).expect("log should walk from the virtual branch head");
    let out = output::end_capture();
    assert!(
        out.contains("initial"),
        "log should include the base commit: {out}"
    );
    assert!(
        out.contains("(older history not cached in this mount)"),
        "log should explain the mount cache boundary: {out}"
    );

    output::begin_capture();
    mount::log_json(dest, Some(10)).expect("JSON log should walk from the virtual branch head");
    let log: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(log.as_array().unwrap().len(), 1);
    assert_eq!(log[0]["branch"], "main");
    assert_eq!(log[0]["description_or_subject"], "initial");
}

#[test]
fn log_reports_empty_mount_history() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    cache.delete_branch(&cfg.virtual_branch).unwrap();

    output::begin_capture();
    mount::log(dest, Some(10), false).expect("empty mount log should not error");
    let out = output::end_capture();
    assert!(out.contains("No commits yet"), "got: {out}");

    output::begin_capture();
    mount::log_json(dest, Some(10)).expect("empty JSON mount log should not error");
    let log: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert!(log.as_array().unwrap().is_empty());
}

#[test]
fn explicit_mount_log_limit_does_not_print_truncation_notes() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let missing_parent = Hash::from_hex(&"2".repeat(64)).unwrap();
    let state_dir =
        build_mount_with_remote_and_parent(dest, "https://oak.example", Some(missing_parent));
    dirty_readme(&state_dir);
    mount::commit(dest).unwrap();

    output::begin_capture();
    mount::log(dest, Some(1), false).expect("limited log should succeed");
    let out = output::end_capture();
    assert!(
        !out.contains("more commits"),
        "explicit -n should own truncation: {out}"
    );
    assert!(
        !out.contains("older history not cached"),
        "limit stopped the walk before the cache boundary: {out}"
    );
}

// ---------------------------------------------------------------------------
// Stale-registration liveness (a registry entry is not a live mount)
// ---------------------------------------------------------------------------

#[test]
fn stale_registration_plans_respawn_not_already_mounted() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let _state_dir = build_mount(dest);

    // Registered, state dir intact, but nothing is mounted (a plain temp dir
    // shares its parent's device) — exactly the post-reboot / daemon-crash
    // state. The mount command must NOT treat this as "already mounted";
    // it should respawn the daemon over the existing state.
    assert_eq!(
        mount::spawn::plan_spawn(dest).unwrap(),
        mount::spawn::SpawnPlan::Respawn,
    );
}

#[test]
fn stale_registration_without_state_is_cleaned_up() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    // The state dir is gone (e.g. manually deleted): nothing to resume from,
    // so the dead registry entry is dropped and a fresh mount proceeds.
    fs::remove_dir_all(&state_dir).unwrap();
    assert_eq!(
        mount::spawn::plan_spawn(dest).unwrap(),
        mount::spawn::SpawnPlan::Fresh,
    );
    assert!(
        mount::state::lookup_id_for(dest).unwrap().is_none(),
        "dead registry entry should have been removed"
    );
}

#[test]
fn unregistered_dest_plans_fresh_mount() {
    let temp = TempDir::new().unwrap();
    // Isolate the index so we don't read the real `~/.oak/mounts/index.json`.
    mount::state::set_mounts_root(temp.path().join("mounts-root"));
    assert_eq!(
        mount::spawn::plan_spawn(&temp.path().join("never-mounted")).unwrap(),
        mount::spawn::SpawnPlan::Fresh,
    );
}

// ---------------------------------------------------------------------------
// `oak mount forget`
// ---------------------------------------------------------------------------

#[test]
fn forget_removes_stale_entry_but_keeps_state() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    mount::forget(dest, false).expect("forget on a stale registration should succeed");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
    assert!(
        state_dir.exists(),
        "forget must not touch on-disk state (it may hold unpushed commits)"
    );
}

#[cfg(unix)]
#[test]
fn forget_refuses_live_mount_without_force() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    // Simulate a live daemon: record this very process's pid.
    mount::state::save_daemon_pid(&state_dir, std::process::id()).unwrap();

    let err = mount::forget(dest, false).expect_err("forget should refuse a live mount");
    let msg = err.to_string();
    assert!(msg.contains("live"), "should say the mount is live: {msg}");
    assert!(msg.contains("--force"), "should mention --force: {msg}");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());

    mount::forget(dest, true).expect("forget --force should override");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Teardown vs committed-but-unpushed work
// ---------------------------------------------------------------------------

/// Dirty the overlay with an edit to README.md so the next `commit` creates a
/// (local-only) commit on the virtual branch.
fn dirty_readme(state_dir: &Path) {
    dirty_overlay_file(state_dir, "README.md", b"# edited\n");
}

fn dirty_overlay_file(state_dir: &Path, path: &str, content: &[u8]) {
    let overlay_file = mount::state::overlay_filename_for(path);
    let overlay_path = mount::state::overlay_dir(state_dir).join(&overlay_file);
    if let Some(parent) = overlay_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(overlay_path, content).unwrap();
    let mut overlay = mount::state::load_overlay_meta(state_dir).unwrap();
    overlay.dirty.insert(
        path.into(),
        mount::state::DirtyEntry {
            overlay_file,
            mode: "regular".into(),
            in_place: false,
        },
    );
    mount::state::save_overlay_meta(state_dir, &overlay).unwrap();
}

async fn expect_mount_commit_push(server: &MockServer, cfg: &mount::state::MountConfig) {
    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": cfg.base_commit.as_str(),
        })))
        .expect(2)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/myrepo/branches/{}",
            cfg.virtual_branch
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "missing": []
        })))
        .expect(0)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/push"))
        .and(body_partial_json(json!({
            "branch": { "name": cfg.virtual_branch.as_str() },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn expect_mount_description_sync(
    server: &MockServer,
    cfg: &mount::state::MountConfig,
    description: &str,
) {
    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": cfg.base_commit.as_str(),
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/push"))
        .and(body_partial_json(json!({
            "branch": {
                "name": cfg.virtual_branch.as_str(),
                "description": description,
            },
            "commits": [],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_clean_pushed_sets_multiline_desc_and_ends() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let description = "# Summary\n\n- preserves `inline code`\n\n```rust\nfn main() {}\n```\n";

    expect_mount_description_sync(&server, &cfg, description).await;

    mount::finish(dest, description)
        .await
        .expect("clean pushed finish should end mount");

    assert!(!state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_dirty_commits_pushes_and_ends() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    dirty_readme(&state_dir);
    let cfg = mount::state::load_config(&state_dir).unwrap();

    expect_mount_commit_push(&server, &cfg).await;

    mount::finish(dest, "finish dirty work")
        .await
        .expect("dirty finish should commit, push, and end");

    assert!(!state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_unpushed_only_pushes_and_ends() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());

    dirty_readme(&state_dir);
    mount::commit(dest).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();

    expect_mount_commit_push(&server, &cfg).await;

    mount::finish(dest, "finish already committed work")
        .await
        .expect("unpushed-only finish should push and end");

    assert!(!state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_push_failure_keeps_dirty_commit_state() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    dirty_readme(&state_dir);
    let cfg = mount::state::load_config(&state_dir).unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": cfg.base_commit.as_str(),
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/myrepo/branches/{}",
            cfg.virtual_branch
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/push"))
        .respond_with(ResponseTemplate::new(500).set_body_string("offline"))
        .expect(1)
        .mount(&server)
        .await;

    let err = mount::finish_json(dest, "finish but push fails")
        .await
        .expect_err("finish should refuse to end when push fails");
    let envelope = output::JsonErrorEnvelope::from_error(&err);
    let json = serde_json::to_value(envelope).unwrap();
    assert_eq!(json["error"]["code"], "finish_phase_failed");
    assert_eq!(json["error"]["finish"]["phase"], "push");
    assert_eq!(
        json["error"]["finish"]["completed_phases"],
        json!(["preflight", "description", "commit"])
    );
    assert_eq!(
        json["error"]["finish"]["pending_phases"],
        json!(["push", "mount_end"])
    );
    assert_eq!(json["error"]["finish"]["retry_command"], "oak push");
    let msg = json["error"]["message"].as_str().unwrap();
    assert!(msg.contains("unpushed"), "should mention unpushed: {msg}");
    assert!(msg.contains("oak push"), "should name next command: {msg}");
    assert!(state_dir.exists(), "state dir must be preserved");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
    assert_eq!(mount::state::unpushed_commit_count(&state_dir), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_retry_after_push_failure_succeeds_and_ends() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    dirty_readme(&state_dir);
    let cfg = mount::state::load_config(&state_dir).unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": cfg.base_commit.as_str(),
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/myrepo/branches/{}",
            cfg.virtual_branch
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/push"))
        .respond_with(ResponseTemplate::new(500).set_body_string("offline"))
        .expect(1)
        .mount(&server)
        .await;

    let err = mount::finish_json(dest, "finish but push fails")
        .await
        .expect_err("first finish should preserve retryable mount state");
    let envelope = output::JsonErrorEnvelope::from_error(&err);
    let json = serde_json::to_value(envelope).unwrap();
    assert_eq!(json["error"]["finish"]["phase"], "push");
    assert!(state_dir.exists());
    assert_eq!(mount::state::unpushed_commit_count(&state_dir), 1);

    let retry_server = MockServer::start().await;
    let mut cfg = mount::state::load_config(&state_dir).unwrap();
    cfg.remote_url = retry_server.uri();
    mount::state::save_config(&state_dir, &cfg).unwrap();
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    cache
        .set_metadata(MetadataKey::RemoteUrl, &cfg.remote_url)
        .unwrap();
    drop(cache);
    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": cfg.base_commit.as_str(),
        })))
        .expect(2)
        .mount(&retry_server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/myrepo/branches/{}",
            cfg.virtual_branch
        )))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&retry_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/push"))
        .and(body_partial_json(json!({
            "branch": { "name": cfg.virtual_branch.as_str() },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&retry_server)
        .await;
    let finished = mount::finish_json(dest, "retry finish succeeds")
        .await
        .expect("second finish should push preserved commit and end mount");
    let finished = serde_json::to_value(finished).unwrap();
    assert_eq!(
        finished["committed"], false,
        "retry must not duplicate the first commit"
    );
    assert_eq!(finished["pushed"], true);
    assert_eq!(finished["ended"], true);
    assert_eq!(finished["unpushed_before"], 1);
    assert_eq!(finished["unpushed_after"], 0);
    assert!(!state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_json_remote_preflight_leaves_description_overlay_and_head_unchanged() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    dirty_readme(&state_dir);
    let cfg = mount::state::load_config(&state_dir).unwrap();
    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(503).set_body_string("offline"))
        .expect(1)
        .mount(&server)
        .await;
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let head_before = cache.get_branch_head(&cfg.virtual_branch).unwrap();
    let branch_before = cache.get_branch(&cfg.virtual_branch).unwrap().unwrap();
    drop(cache);

    let err = mount::finish_json(dest, "new mount finish description")
        .await
        .expect_err("remote preflight should fail before mutation");

    let envelope = output::JsonErrorEnvelope::from_error(&err);
    let json = serde_json::to_value(envelope).unwrap();
    assert_eq!(json["error"]["code"], "finish_preflight_failed");
    assert_eq!(json["error"]["finish"]["phase"], "preflight");
    assert_eq!(json["error"]["finish"]["blocker"], "remote_unreachable");
    assert_eq!(
        json["error"]["finish"]["pending_phases"],
        json!(["description", "commit", "push", "mount_end"])
    );
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let branch_after = cache.get_branch(&cfg.virtual_branch).unwrap().unwrap();
    assert_eq!(branch_after.description, branch_before.description);
    assert_eq!(
        cache.get_branch_head(&cfg.virtual_branch).unwrap(),
        head_before
    );
    assert!(!mount::state::load_overlay_meta(&state_dir)
        .unwrap()
        .dirty
        .is_empty());
    assert!(state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "preflight failure must not reach commit/push/metadata phases"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_json_auth_preflight_leaves_description_overlay_and_head_unchanged() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote_without_auth(dest, &server.uri());
    dirty_readme(&state_dir);
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let head_before = cache.get_branch_head(&cfg.virtual_branch).unwrap();
    let branch_before = cache.get_branch(&cfg.virtual_branch).unwrap().unwrap();

    let err = mount::finish_json(dest, "new description without login")
        .await
        .expect_err("finish should fail auth preflight before mutation");

    let envelope = output::JsonErrorEnvelope::from_error(&err);
    assert_eq!(envelope.error.code, "finish_preflight_failed");
    let finish = envelope.error.finish.expect("finish details");
    assert_eq!(finish.phase, "preflight");
    assert_eq!(finish.blocker.as_deref(), Some("auth_missing"));
    assert_eq!(
        finish.pending_phases,
        vec!["description", "commit", "push", "mount_end"]
    );
    let expected_login = format!("oak login -r {}", server.uri());
    assert_eq!(
        finish.retry_command.as_deref(),
        Some(expected_login.as_str())
    );

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let branch_after = cache.get_branch(&cfg.virtual_branch).unwrap().unwrap();
    assert_eq!(branch_after.description, branch_before.description);
    assert_eq!(
        cache.get_branch_head(&cfg.virtual_branch).unwrap(),
        head_before
    );
    assert!(
        mount::state::load_overlay_meta(&state_dir)
            .unwrap()
            .dirty
            .contains_key("README.md"),
        "dirty overlay must survive auth preflight failure"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "auth preflight must fail before contacting the remote"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_json_metadata_sync_failure_reports_phase_after_description() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    let cfg = mount::state::load_config(&state_dir).unwrap();
    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": cfg.base_commit.as_str(),
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/push"))
        .respond_with(ResponseTemplate::new(500).set_body_string("metadata offline"))
        .expect(1)
        .mount(&server)
        .await;

    let err = mount::finish_json(dest, "description survives metadata failure")
        .await
        .expect_err("metadata sync should fail after description phase");

    let envelope = output::JsonErrorEnvelope::from_error(&err);
    let json = serde_json::to_value(envelope).unwrap();
    assert_eq!(json["error"]["code"], "finish_phase_failed");
    assert_eq!(json["error"]["finish"]["phase"], "metadata_sync");
    assert_eq!(
        json["error"]["finish"]["completed_phases"],
        json!(["preflight", "description"])
    );
    assert_eq!(
        json["error"]["finish"]["pending_phases"],
        json!(["metadata_sync", "mount_end"])
    );
    assert_eq!(
        json["error"]["finish"]["retry_command"],
        "oak mount finish <path> --desc-file <file> --json"
    );
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let branch = cache.get_branch(&cfg.virtual_branch).unwrap().unwrap();
    assert_eq!(
        branch.description.as_deref(),
        Some("description survives metadata failure")
    );
    assert!(state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
}

#[test]
fn mount_finish_resolves_nested_mount_path() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);
    let sub = dest.join("nested");
    fs::create_dir_all(&sub).unwrap();

    let resolved = mount::mount_dest_for(&sub).unwrap();
    assert_eq!(resolved, Some(dest.canonicalize().unwrap()));
    assert!(state_dir.exists());
}

#[test]
fn mount_list_json_parses_and_reports_mount_state() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    dirty_readme(&state_dir);
    mount::commit(dest).unwrap();

    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.deletions.push("docs/old.md".into());
    overlay
        .renames
        .insert("src/old.rs".into(), "src/new.rs".into());
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    output::begin_capture();
    mount::list(true).unwrap();
    let out = output::end_capture();
    let json: Value = serde_json::from_str(&out).expect("mount list JSON should parse");
    let mounts = json.as_array().expect("mount list JSON should be an array");
    assert_eq!(mounts.len(), 1);

    let mount = &mounts[0];
    let canonical_dest = std::fs::canonicalize(dest).unwrap();
    assert_eq!(
        mount["mount_point"],
        canonical_dest.to_string_lossy().as_ref()
    );
    assert_eq!(mount["repo"]["spec"], "oak/myrepo");
    assert_eq!(mount["repo"]["owner"], "oak");
    assert_eq!(mount["repo"]["name"], "myrepo");
    assert_eq!(mount["remote_url"], "https://oak.example");
    assert_eq!(mount["base"]["branch"], "main");
    assert!(mount["base"]["commit"].as_str().unwrap().len() >= 40);
    assert!(mount["virtual_branch"]
        .as_str()
        .unwrap()
        .starts_with("main--mount-"));
    assert_eq!(mount["status"], "stale");
    assert_eq!(mount["dirty_overlay"]["deleted"], 1);
    assert_eq!(mount["dirty_overlay"]["renamed"], 1);
    assert_eq!(mount["dirty_overlay"]["deleted_paths"][0], "docs/old.md");
    assert_eq!(
        mount["dirty_overlay"]["renamed_paths"][0]["from"],
        "src/old.rs"
    );
    assert_eq!(mount["unpushed_commits"], 1);
}

#[test]
fn mount_structured_status_info_log_and_agent_state_parse() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    output::begin_capture();
    mount::agent_state_json(dest, false, false).unwrap();
    let clean_state: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(clean_state["schema_version"], AGENT_STATE_SCHEMA_VERSION);
    assert_eq!(clean_state["context"], "mount");
    assert_eq!(clean_state["dirty"], false);
    assert_eq!(
        clean_state["recommended_next_commands"][0],
        "oak finish --desc-file <file> --json"
    );

    dirty_readme(&state_dir);

    output::begin_capture();
    mount::status_json(dest).unwrap();
    let status: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(
        status["branch"].as_str().unwrap(),
        status["mount"]["virtual_branch"]
    );
    assert_eq!(status["dirty_overlay"]["modified_or_created"], 1);
    assert_eq!(status["changes"][0]["path"], "README.md");
    assert_eq!(status["progress_state"]["in_progress"], false);

    output::begin_capture();
    mount::status_compact_json(dest).unwrap();
    let compact: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(compact["schema_version"], 1);
    assert_eq!(compact["dirty"], true);
    assert_eq!(compact["change_count"], 1);
    assert_eq!(compact["change_counts"]["modified"], 1);
    assert_eq!(compact["changes"][0]["path"], "README.md");
    assert_eq!(compact["changes"][0]["status"], "modified");
    assert_eq!(
        compact["mount"]["virtual_branch"],
        status["mount"]["virtual_branch"]
    );
    assert_eq!(compact["mount"]["dirty_overlay"]["modified_or_created"], 1);
    assert!(compact.get("dirty_overlay").is_none());
    assert!(
        compact.get("progress_state").is_none(),
        "idle compact mount status should omit default progress_state"
    );

    output::begin_capture();
    mount::status_porcelain(dest).unwrap();
    let porcelain = output::end_capture();
    assert!(porcelain.contains("M README.md"), "porcelain: {porcelain}");

    output::begin_capture();
    mount::info_json(dest).unwrap();
    let info: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(info["repo_owner"], "oak");
    assert_eq!(info["repo_name"], "myrepo");
    assert_eq!(info["mount"]["repo"]["spec"], "oak/myrepo");
    let virtual_branch = info["mount"]["virtual_branch"].as_str().unwrap();

    output::begin_capture();
    mount::info(dest).unwrap();
    let info_text = output::end_capture();
    assert!(
        info_text.contains("Repository: oak/myrepo"),
        "info: {info_text}"
    );
    assert!(
        info_text.contains("Remote: https://oak.example"),
        "info: {info_text}"
    );
    assert!(info_text.contains("Mount: "), "info: {info_text}");
    assert!(
        info_text.contains(&format!("Branch: {virtual_branch}")),
        "info: {info_text}"
    );
    assert!(
        info_text.contains("Dirty overlay: 1 modified/created, 0 deleted, 0 renamed"),
        "info: {info_text}"
    );
    assert!(info_text.contains("Progress: none"), "info: {info_text}");

    output::begin_capture();
    mount::agent_state_json(dest, false, false).unwrap();
    let dirty_state: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(dirty_state["schema_version"], AGENT_STATE_SCHEMA_VERSION);
    assert_eq!(dirty_state["context"], "mount");
    assert_eq!(dirty_state["dirty"], true);
    assert_eq!(
        dirty_state["recommended_next_commands"][0],
        "oak finish --desc-file <file> --json"
    );
    assert_eq!(dirty_state["recommended_next_commands"][1], "oak commit");

    mount::commit(dest).unwrap();

    output::begin_capture();
    mount::hash(dest).unwrap();
    let mount_hash = output::end_capture();
    assert!(mount_hash.trim().len() >= 40);

    output::begin_capture();
    mount::rev_parse_head(dest, true).unwrap();
    let short_hash = output::end_capture();
    assert_eq!(short_hash.trim().len(), 12);
    assert!(mount_hash.starts_with(short_hash.trim()));

    output::begin_capture();
    mount::show_current_branch(dest).unwrap();
    let current_branch = output::end_capture();
    assert_eq!(
        current_branch,
        info["mount"]["virtual_branch"].as_str().unwrap()
    );

    output::begin_capture();
    mount::log_json(dest, Some(1)).unwrap();
    let log: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(log.as_array().unwrap().len(), 1);
    assert!(log[0]["hash"].as_str().unwrap().len() >= 40);

    output::begin_capture();
    mount::agent_state_json(dest, false, false).unwrap();
    let state: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(state["context"], "mount");
    assert_eq!(state["mount"]["repo"]["name"], "myrepo");
    assert_eq!(state["unpushed_commit_count"], 1);
    assert_eq!(
        state["recommended_next_commands"][0],
        "oak finish --desc-file <file> --json"
    );
    assert_eq!(state["recommended_next_commands"][1], "oak push");

    output::begin_capture();
    mount::agent_state_json(dest, true, false).unwrap();
    let refreshed: Value = serde_json::from_str(&output::end_capture()).unwrap();
    assert_eq!(refreshed["refresh_requested"], true);
    assert_eq!(refreshed["refresh_supported"], false);
    assert_eq!(refreshed["current_branch_push_checked"], false);
    assert!(refreshed["refresh_errors"][0]
        .as_str()
        .unwrap()
        .contains("not supported inside mounts"));
}

#[test]
fn cli_status_json_compact_routes_inside_mount() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);
    dirty_readme(&state_dir);
    let mounts_root = mount::state::mounts_root().unwrap();

    let out = oak_with_mounts_root(dest, &["status", "--json", "--compact"], &mounts_root);
    assert!(
        out.status.success(),
        "status --json --compact failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be compact JSON");

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["dirty"], true);
    assert_eq!(json["change_count"], 1);
    assert_eq!(json["change_counts"]["modified"], 1);
    assert_eq!(json["changes"][0]["path"], "README.md");
    assert_eq!(json["mount"]["repo"]["spec"], "oak/myrepo");
    assert!(stderr(&out).is_empty(), "stderr should be empty");
}

#[test]
fn cli_diff_json_routes_inside_mount() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);
    dirty_readme(&state_dir);
    let mounts_root = mount::state::mounts_root().unwrap();

    let out = oak_with_mounts_root(dest, &["diff", "--json"], &mounts_root);

    assert!(
        out.status.success(),
        "diff --json failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(stderr(&out), "");
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be diff JSON");

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["kind"], "working_tree_diff");
    assert_eq!(json["diff_mode"], "working_tree");
    assert_eq!(json["branch"], json["mount"]["virtual_branch"]);
    assert_eq!(json["against"], "HEAD");
    assert_eq!(json["parent"], "main");
    assert_eq!(json["changed_file_count"], 1);
    assert_eq!(json["changed_files"][0]["path"], "README.md");
    assert_eq!(json["changed_files"][0]["status"], "modified");
    assert_eq!(json["changed_files"][0]["additions"], 1);
    assert_eq!(json["changed_files"][0]["deletions"], 1);
    assert_eq!(json["mount"]["repo"]["spec"], "oak/myrepo");
    assert_eq!(json["mount"]["dirty_overlay"]["modified_or_created"], 1);
    assert_eq!(json["recommended_next_commands"][0], "oak diff --print");
}

#[tokio::test(flavor = "current_thread")]
async fn remote_branch_inspection_routes_inside_mount() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let main_head = cache
        .get_branch_head("main")
        .unwrap()
        .expect("mount cache has main head");
    let feature_branch = Branch::new(
        "remote-feature".to_string(),
        Some("remote branch".to_string()),
        Some("main".to_string()),
    );
    cache.store_branch(&feature_branch).unwrap();
    cache
        .set_branch_head("remote-feature", &main_head)
        .expect("feature branch has a cached head");
    drop(cache);

    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "branches": [
                {
                    "name": "main",
                    "description": "main",
                    "parent_branch": null,
                    "status": "open",
                    "close_reason": null,
                    "head": main_head.as_str(),
                    "created_at": "2026-07-08T00:00:00Z"
                },
                {
                    "name": "remote-feature",
                    "description": "remote branch",
                    "parent_branch": "main",
                    "status": "open",
                    "close_reason": null,
                    "head": main_head.as_str(),
                    "created_at": "2026-07-08T00:00:00Z"
                }
            ]
        })))
        .expect(2)
        .mount(&server)
        .await;

    let mounts_root = mount::state::mounts_root().unwrap();
    let list = oak_with_mounts_root(
        dest,
        &["branch", "list", "--remote", "--status", "open", "--json"],
        &mounts_root,
    );
    assert!(
        list.status.success(),
        "branch list --remote failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&list),
        stderr(&list)
    );
    let branches: Value = serde_json::from_str(&stdout(&list)).expect("list stdout is JSON");
    assert!(branches
        .as_array()
        .unwrap()
        .iter()
        .any(|branch| branch["name"] == "remote-feature" && branch["remote"] == true));

    let show = oak_with_mounts_root(
        dest,
        &["branch", "show", "remote-feature", "--remote", "--json"],
        &mounts_root,
    );
    assert!(
        show.status.success(),
        "branch show --remote failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&show),
        stderr(&show)
    );
    let shown: Value = serde_json::from_str(&stdout(&show)).expect("show stdout is JSON");
    assert_eq!(shown["name"], "remote-feature");
    assert_eq!(shown["head"], main_head.as_str());
    assert_eq!(shown["current"], false);

    let cfg_after = mount::state::load_config(&state_dir).unwrap();
    assert_eq!(
        cfg_after.virtual_branch, cfg.virtual_branch,
        "remote branch inspection from a mount must not rewrite mount state"
    );
    assert!(state_dir.exists());
    assert!(stderr(&list).is_empty(), "stderr should be empty");
    assert!(stderr(&show).is_empty(), "stderr should be empty");
}

/// Filtered `oak log` (paths, `-S`, `-G`) is not implemented inside mounts.
/// The refusal must hold for `--json` too: answering a filtered query with
/// unfiltered history at exit 0 would silently lie to the caller. JSON mode
/// gets the standard error envelope on stdout, exit 2.
#[test]
fn cli_log_json_filters_error_honestly_inside_mount() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    build_mount(dest);
    let mounts_root = mount::state::mounts_root().unwrap();

    for args in [
        vec!["log", "--json", "-G", "pattern"],
        vec!["log", "--json", "-S", "term"],
        vec!["log", "--json", "README.md"],
    ] {
        let out = oak_with_mounts_root(dest, &args, &mounts_root);
        assert_eq!(
            out.status.code(),
            Some(2),
            "filtered log --json must be a usage error inside a mount ({args:?})\nstdout:\n{}\nstderr:\n{}",
            stdout(&out),
            stderr(&out)
        );
        let json: Value =
            serde_json::from_str(&stdout(&out)).expect("stdout should be a JSON error envelope");
        assert_eq!(json["error"]["code"], "invalid_argument", "args: {args:?}");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not supported inside mounts"),
            "args {args:?}: {json}"
        );
    }

    // Unfiltered `log --json` still works inside the mount.
    let out = oak_with_mounts_root(dest, &["log", "--json"], &mounts_root);
    assert!(
        out.status.success(),
        "plain log --json should still work\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
}

#[test]
fn cli_diff_json_inside_mount_supports_changed_file_paging() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);
    dirty_readme(&state_dir);
    dirty_overlay_file(&state_dir, "docs/notes.md", b"notes\n");

    let mounts_root = mount::state::mounts_root().unwrap();
    let out = oak_with_mounts_root(
        dest,
        &["diff", "--json", "--changed-files-limit", "1"],
        &mounts_root,
    );

    assert!(
        out.status.success(),
        "paged diff --json failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be diff JSON");
    assert_eq!(json["changed_file_count"], 2);
    assert_eq!(json["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(json["changed_files_page"]["offset"], 0);
    assert_eq!(json["changed_files_page"]["limit"], 1);
    assert_eq!(json["changed_files_page"]["total_count"], 2);
    assert_eq!(json["changed_files_page"]["returned_count"], 1);
    assert_eq!(json["changed_files_page"]["omitted_count"], 1);
    assert_eq!(json["changed_files_page"]["next_offset"], 1);
    assert!(json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd == "oak diff --json"));

    let out = oak_with_mounts_root(
        dest,
        &[
            "diff",
            "--json",
            "--changed-files-limit",
            "1",
            "--changed-files-offset",
            "1",
        ],
        &mounts_root,
    );
    assert!(
        out.status.success(),
        "offset paged diff --json failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be diff JSON");
    assert_eq!(json["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(json["changed_files_page"]["offset"], 1);
    assert_eq!(json["changed_files_page"]["limit"], 1);
    assert_eq!(json["changed_files_page"]["total_count"], 2);
    assert_eq!(json["changed_files_page"]["returned_count"], 1);
    assert_eq!(json["changed_files_page"]["omitted_count"], 1);
    assert!(json["changed_files_page"]["next_offset"].is_null());

    let out = oak_with_mounts_root(
        dest,
        &["diff", "--json", "--changed-files-offset", "1"],
        &mounts_root,
    );
    assert!(
        out.status.success(),
        "offset-only diff --json failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be diff JSON");
    assert_eq!(json["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(json["changed_files_page"]["offset"], 1);
    assert!(json["changed_files_page"]["limit"].is_null());
    assert_eq!(json["changed_files_page"]["total_count"], 2);
    assert_eq!(json["changed_files_page"]["returned_count"], 1);
    assert_eq!(json["changed_files_page"]["omitted_count"], 1);
    assert!(json["changed_files_page"]["next_offset"].is_null());
}

#[test]
fn cli_diff_json_inside_mount_counts_nul_utf8_like_regular_diff() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);
    dirty_overlay_file(&state_dir, "README.md", b"# hello\0\n");
    let mounts_root = mount::state::mounts_root().unwrap();

    let out = oak_with_mounts_root(dest, &["diff", "--json"], &mounts_root);

    assert!(
        out.status.success(),
        "diff --json failed\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let json: Value = serde_json::from_str(&stdout(&out)).expect("stdout should be diff JSON");
    let file = &json["changed_files"][0];
    assert_eq!(file["path"], "README.md");
    assert_eq!(file["additions"], 1);
    assert_eq!(file["deletions"], 1);
    assert!(file["binary_or_large"].is_null());
    assert!(file["stats_available"].is_null());
}

#[tokio::test(flavor = "current_thread")]
async fn mount_finish_json_reports_result_and_ends() {
    let server = MockServer::start().await;
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount_with_remote(dest, &server.uri());
    dirty_readme(&state_dir);
    let cfg = mount::state::load_config(&state_dir).unwrap();

    expect_mount_commit_push(&server, &cfg).await;

    let result = mount::finish_json(dest, "finish json work").await.unwrap();
    let json = serde_json::to_value(result).unwrap();

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["repo"]["spec"], "oak/myrepo");
    assert_eq!(json["virtual_branch"], cfg.virtual_branch);
    assert_eq!(json["committed"], true);
    assert_eq!(json["pushed"], true);
    assert_eq!(json["ended"], true);
    assert_eq!(json["dirty_overlay"]["modified_or_created"], 1);
    assert_eq!(json["unpushed_after"], 0);
    assert!(json["branch_url"].as_str().unwrap().contains("/branches/"));
    assert!(!state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[test]
fn mount_end_refusal_dirty_overlay_diagnostics() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    dirty_readme(&state_dir);
    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.deletions.push("docs/old.md".into());
    overlay
        .renames
        .insert("src/old.rs".into(), "src/new.rs".into());
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    let err = mount::end(dest, false).expect_err("end should reject dirty mount");
    let msg = err.to_string();
    assert!(
        msg.contains("1 modified/created"),
        "should count modified paths: {msg}"
    );
    assert!(
        msg.contains("README.md"),
        "should name modified path: {msg}"
    );
    assert!(msg.contains("1 deleted"), "should count deletions: {msg}");
    assert!(
        msg.contains("docs/old.md"),
        "should name deleted path: {msg}"
    );
    assert!(msg.contains("1 renamed"), "should count renames: {msg}");
    assert!(
        msg.contains("src/old.rs -> src/new.rs"),
        "should name rename: {msg}"
    );
    assert!(
        msg.contains("oak status"),
        "should point at oak status: {msg}"
    );
    assert!(
        msg.contains("oak commit"),
        "should point at oak commit: {msg}"
    );
    assert!(msg.contains("oak push"), "should point at oak push: {msg}");
    assert!(
        msg.contains("oak mount end --force"),
        "should name force teardown command: {msg}"
    );
    assert!(state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
}

#[test]
fn mount_end_refusal_unpushed_commit_diagnostics() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    dirty_readme(&state_dir);
    mount::commit(dest).unwrap();

    let err = mount::end(dest, false).expect_err("end should refuse unpushed commits");
    let msg = err.to_string();
    assert!(
        msg.contains("1 unpushed commit(s)"),
        "should count unpushed commits: {msg}"
    );
    assert!(
        msg.contains("main--mount-"),
        "should name virtual branch: {msg}"
    );
    assert!(msg.contains("oak push"), "should point at oak push: {msg}");
    assert!(
        msg.contains("oak mount end --force"),
        "should name force teardown command: {msg}"
    );
    assert!(state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
}

#[test]
fn end_refuses_unpushed_commits_without_force() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    dirty_readme(&state_dir);
    mount::commit(dest).unwrap();

    // The overlay is clean now, but the commit lives solely in this state
    // dir's cache.db — `end` must refuse to delete the only copy.
    let err = mount::end(dest, false).expect_err("end should refuse unpushed commits");
    let msg = err.to_string();
    assert!(msg.contains("unpushed"), "should mention unpushed: {msg}");
    assert!(msg.contains("oak push"), "should point at oak push: {msg}");
    assert!(state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());

    // Once the head is recorded as pushed, the same mount ends cleanly.
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let head = cache.get_branch_head(&cfg.virtual_branch).unwrap().unwrap();
    drop(cache);
    mount::state::save_pushed_head(&state_dir, head.as_str()).unwrap();

    mount::end(dest, false).expect("end after push should succeed");
    assert!(!state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[test]
fn end_force_discards_unpushed_commits() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    dirty_readme(&state_dir);
    mount::commit(dest).unwrap();

    mount::end(dest, true).expect("end --force should discard unpushed commits");
    assert!(!state_dir.exists());
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

#[test]
fn worktree_remove_leaves_unpushed_mount_in_place() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    dirty_readme(&state_dir);
    mount::commit(dest).unwrap();

    // The hook can't block removal, so it returns Ok — but it must not have
    // torn down the mount holding the only copy of the commit.
    mount::worktree::worktree_remove_at(dest).expect("hook reports success");
    assert!(state_dir.exists(), "state dir must survive the hook");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
}

#[test]
fn worktree_remove_leaves_dirty_mount_in_place() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    dirty_readme(&state_dir);

    mount::worktree::worktree_remove_at(dest).expect("hook reports success");
    assert!(state_dir.exists(), "state dir must survive the hook");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_some());
}

#[test]
fn worktree_remove_ends_clean_pushed_mount() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    mount::worktree::worktree_remove_at(dest).expect("hook reports success");
    assert!(!state_dir.exists(), "clean mount should be torn down");
    assert!(mount::state::lookup_id_for(dest).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn end_terminates_recorded_daemon() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    // Stand-in for the parked mount daemon: a process that would outlive the
    // teardown unless `end` terminates it via the recorded pid.
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep");
    mount::state::save_daemon_pid(&state_dir, child.id()).unwrap();

    mount::end(dest, false).expect("end on clean mount should succeed");
    assert!(!state_dir.exists());

    // The recorded daemon must be gone shortly after teardown.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("daemon process should have been terminated by `end`");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Cross-process index safety
// ---------------------------------------------------------------------------

#[test]
fn concurrent_registrations_all_survive() {
    // One shared mounts root across many registering threads, each with its
    // own state handle (the per-thread override must be set per thread).
    // The advisory file lock — not an in-process mutex — is what serializes
    // the index read-modify-write, so this exercises the same path two
    // separate processes would take.
    let root = TempDir::new().unwrap();
    let root_path = root.path().to_path_buf();
    let dests = TempDir::new().unwrap();

    let mut handles = Vec::new();
    for i in 0..8 {
        let root_path = root_path.clone();
        let dest = dests.path().join(format!("task-{i}"));
        fs::create_dir_all(&dest).unwrap();
        handles.push(std::thread::spawn(move || {
            mount::state::set_mounts_root(root_path);
            mount::state::register_mount(&dest, &format!("id-{i}")).unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    mount::state::set_mounts_root(root_path);
    let idx = mount::state::load_index().unwrap();
    assert_eq!(
        idx.mounts.len(),
        8,
        "every concurrent registration must survive: {:?}",
        idx.mounts
    );
}

#[test]
fn log_after_commit_includes_new_commit() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    // Same setup as `commit_picks_up_dirty_overlay_file` — write a dirty
    // overlay, commit, then verify log can find the resulting commit
    // without erroring.
    let new_content = b"# updated\n".to_vec();
    let overlay_file = mount::state::overlay_filename_for("README.md");
    fs::write(
        mount::state::overlay_dir(&state_dir).join(&overlay_file),
        &new_content,
    )
    .unwrap();
    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.dirty.insert(
        "README.md".into(),
        mount::state::DirtyEntry {
            overlay_file,
            mode: "regular".into(),
            in_place: false,
        },
    );
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    mount::commit(dest).unwrap();
    mount::log(dest, None, false).unwrap();

    // After commit, log should run and the cache should have at least one
    // commit object on the virtual branch.
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let commits = cache.get_commits_for_branch(&cfg.virtual_branch).unwrap();
    assert!(
        !commits.is_empty(),
        "should have a commit on virtual branch"
    );
}

/// `oak commit <paths>` inside a mount lands only the selected overlay
/// entries; the rest stay dirty (meta and overlay files intact) for a later
/// commit.
#[test]
fn scoped_commit_lands_only_selected_overlay_entries() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    // Two dirty files: an edit of the base README.md and a brand-new file.
    let overlay_root = mount::state::overlay_dir(&state_dir);
    let readme_overlay = mount::state::overlay_filename_for("README.md");
    let notes_overlay = mount::state::overlay_filename_for("docs/notes.md");
    fs::write(overlay_root.join(&readme_overlay), b"# edited\n").unwrap();
    fs::write(overlay_root.join(&notes_overlay), b"notes\n").unwrap();
    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.dirty.insert(
        "README.md".into(),
        mount::state::DirtyEntry {
            overlay_file: readme_overlay,
            mode: "regular".into(),
            in_place: false,
        },
    );
    overlay.dirty.insert(
        "docs/notes.md".into(),
        mount::state::DirtyEntry {
            overlay_file: notes_overlay.clone(),
            mode: "regular".into(),
            in_place: false,
        },
    );
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    // Commit only README.md.
    mount::commit_paths(dest, dest, &[dest.join("README.md")]).expect("scoped commit");

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let head = cache.get_branch_head(&cfg.virtual_branch).unwrap().unwrap();
    let commit = cache.get_commit(&head).unwrap().unwrap();
    assert_ne!(head.as_str(), cfg.base_commit, "head should advance");

    // The new manifest carries the edited README but not the unselected file.
    let manifest = cache.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["README.md"]);
    let readme = cache
        .get_blob(&manifest.entries[0].blob_hash)
        .unwrap()
        .unwrap();
    assert_eq!(readme.content, b"# edited\n");

    // The unselected entry is still dirty: meta survives, overlay file intact.
    let post = mount::state::load_overlay_meta(&state_dir).unwrap();
    assert!(post.dirty.contains_key("docs/notes.md"));
    assert!(!post.dirty.contains_key("README.md"));
    assert!(
        overlay_root.join(&notes_overlay).exists(),
        "uncommitted overlay file must survive a scoped commit"
    );

    // A follow-up full commit sweeps the rest and empties the overlay.
    mount::commit(dest).expect("full commit");
    let post = mount::state::load_overlay_meta(&state_dir).unwrap();
    assert!(post.dirty.is_empty());
    let head2 = cache.get_branch_head(&cfg.virtual_branch).unwrap().unwrap();
    let commit2 = cache.get_commit(&head2).unwrap().unwrap();
    let manifest2 = cache.get_manifest(&commit2.manifest_hash).unwrap().unwrap();
    let mut paths2: Vec<&str> = manifest2.entries.iter().map(|e| e.path.as_str()).collect();
    paths2.sort();
    assert_eq!(paths2, vec!["README.md", "docs/notes.md"]);
}

/// A scoped mount commit whose paths match nothing must not advance the head
/// or touch the overlay.
#[test]
fn scoped_commit_with_no_matching_overlay_is_a_noop() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let state_dir = build_mount(dest);

    let overlay_file = mount::state::overlay_filename_for("README.md");
    fs::write(
        mount::state::overlay_dir(&state_dir).join(&overlay_file),
        b"# edited\n",
    )
    .unwrap();
    let mut overlay = mount::state::load_overlay_meta(&state_dir).unwrap();
    overlay.dirty.insert(
        "README.md".into(),
        mount::state::DirtyEntry {
            overlay_file,
            mode: "regular".into(),
            in_place: false,
        },
    );
    mount::state::save_overlay_meta(&state_dir, &overlay).unwrap();

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    let cfg = mount::state::load_config(&state_dir).unwrap();
    let head_before = cache.get_branch_head(&cfg.virtual_branch).unwrap();

    mount::commit_paths(dest, dest, &[dest.join("docs")]).expect("no-match scoped commit");

    let head_after = cache.get_branch_head(&cfg.virtual_branch).unwrap();
    assert_eq!(head_before, head_after, "branch head should not move");
    let post = mount::state::load_overlay_meta(&state_dir).unwrap();
    assert!(post.dirty.contains_key("README.md"), "overlay untouched");
}
