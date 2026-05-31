//! End-to-end-ish tests for the mount lifecycle that don't need FUSE itself.
//!
//! We construct a mount state directory by hand (the same shape `start()`
//! produces), simulate an overlay write, then call `commit` / `status` /
//! `forget` and verify the side-effects.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use oak_cli::commands::mount;
use oak_core::{Branch, BranchStatus, ManifestEntry, MetadataKey};
use oak_core::{Repository, SqliteRepository};
use tempfile::TempDir;

/// Set `OAK_MOUNTS_ROOT` once per test process to a leaked temp dir, so
/// every mount-lifecycle test stores state under an isolated root rather
/// than the user's `~/.oak/mounts/`.
fn isolated_root() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let temp = TempDir::new().expect("temp dir for mounts root");
        // SAFETY: Rust 1.79+ marked `set_var` unsafe. We only call it here,
        // before any background threads are spawned by these tests, so the
        // typical race conditions don't apply.
        unsafe {
            std::env::set_var("OAK_MOUNTS_ROOT", temp.path());
        }
        std::mem::forget(temp); // keep the dir alive for the test process
    });
}

/// Build a mount state dir for `dest`, with one base file `README.md` whose
/// blob is pre-cached. Returns the state-dir path.
fn build_mount(dest: &Path) -> std::path::PathBuf {
    isolated_root();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let state_dir = mount::state::state_dir_for(&id).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(mount::state::overlay_dir(&state_dir)).unwrap();

    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    cache
        .set_metadata(MetadataKey::RemoteUrl, "https://oakvcs.example")
        .unwrap();
    cache.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    cache.set_metadata(MetadataKey::RepoName, "myrepo").unwrap();

    let base = Branch {
        name: "main".to_string(),
        description: None,
        parent_branch: None,
        status: BranchStatus::Open,
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
            None,
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
        remote_url: "https://oakvcs.example".into(),
        owner: "oak".into(),
        repo: "myrepo".into(),
        base_branch: "main".into(),
        base_commit: base_commit_hash.as_str().to_string(),
        virtual_branch,
        team: None,
        project: None,
        path_prefixes: Vec::new(),
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

    let err = mount::forget(dest).expect_err("forget should reject dirty mount");
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

    mount::forget(dest).expect("forget on clean mount should succeed");
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
    // A fresh temp dir that's never been registered shouldn't resolve.
    let temp = TempDir::new().unwrap();
    let resolved = mount::mount_dest_for(temp.path()).unwrap();
    assert!(resolved.is_none());
}

#[test]
fn hash_prints_virtual_branch_head_when_clean() {
    let temp_mnt = TempDir::new().unwrap();
    let dest = temp_mnt.path();
    let _state_dir = build_mount(dest);

    // Hash should succeed on a clean mount; we don't capture stdout here,
    // but the surface contract is "doesn't error". Subsequent log/diff
    // also exercise the same cache-open path.
    mount::hash(dest).expect("hash on a clean mount should succeed");
    mount::log(dest, Some(10)).expect("log on a clean mount should succeed");
    mount::diff(dest).expect("diff on a clean mount should succeed");
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
    mount::log(dest, None).unwrap();

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

#[test]
fn shorthand_space_picks_repo_leaf() {
    let tmp = TempDir::new().unwrap();
    let dest = mount::shorthand_space(tmp.path(), "myrepo", "main").unwrap();
    assert_eq!(dest, tmp.path().join("myrepo"));
}

#[test]
fn shorthand_space_falls_back_when_repo_leaf_busy() {
    let tmp = TempDir::new().unwrap();
    // Make the primary destination a non-empty dir.
    let primary = tmp.path().join("myrepo");
    fs::create_dir_all(&primary).unwrap();
    fs::write(primary.join("placeholder"), b"x").unwrap();

    let dest = mount::shorthand_space(tmp.path(), "myrepo", "main").unwrap();
    assert_eq!(dest, tmp.path().join("myrepo-main"));
}
