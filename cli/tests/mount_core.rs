//! Construction-level tests for `MountCore`: the in-memory tree it builds from
//! a manifest plus the mount's overlay (deletions, renames, dirty files), and
//! — as a regression — that it can be constructed from inside an async runtime
//! without panicking.
//!
//! No FUSE/FSKit mount is involved, so these run in CI on any Unix.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use oak_cli::commands::mount::core::{DirEntry, EntryKind, MountCore, ROOT_INODE};
use oak_cli::commands::mount::state::{self, MountConfig};
use oak_core::{
    Blob, FileChange, FileMode, Hash, Manifest, ManifestEntry, Repository, SqliteRepository,
};
use tempfile::TempDir;

/// Everything `MountCore::new` consumes, plus the temp dir keeping it alive.
struct Seed {
    _dir: TempDir,
    state_dir: PathBuf,
    cache: Arc<SqliteRepository>,
    manifest: Manifest,
    sizes: HashMap<String, u64>,
    cfg: MountConfig,
}

/// Build an isolated mount cache under a fresh temp state dir, seeded with one
/// commit on virtual branch `vb` containing `files`. Each `extra` entry is
/// added to the manifest *without* storing its blob — used to simulate a
/// not-yet-hydrated file (e.g. an ignore file the mount hasn't fetched).
fn seed(files: &[(&str, &[u8])], extra: &[(&str, Hash)]) -> Seed {
    let dir = TempDir::new().unwrap();
    let state_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(state::overlay_dir(&state_dir)).unwrap();
    // `open_relaxed` lets manifest entries reference blobs we haven't stored,
    // matching how a real mount records the manifest before lazy-fetching.
    let cache =
        Arc::new(SqliteRepository::open_relaxed(&state::cache_db_path(&state_dir)).unwrap());

    let mut entries = Vec::new();
    let mut sizes = HashMap::new();
    for (path, content) in files {
        let blob = Blob::new(content.to_vec());
        sizes.insert(blob.hash.as_str().to_string(), blob.size);
        cache.store_blob(&blob).unwrap();
        entries.push(ManifestEntry {
            path: (*path).to_string(),
            blob_hash: blob.hash.clone(),
            mode: FileMode::Regular,
        });
    }
    for (path, hash) in extra {
        entries.push(ManifestEntry {
            path: (*path).to_string(),
            blob_hash: hash.clone(),
            mode: FileMode::Regular,
        });
    }

    let manifest = Manifest::new(entries);
    let mh = cache.put_manifest(manifest.entries.clone()).unwrap();
    let no_files: Vec<FileChange> = Vec::new();
    let head = cache
        .put_commit(
            "vb".into(),
            None,
            None,
            mh,
            "tester".into(),
            None,
            chrono::Utc::now(),
            no_files,
        )
        .unwrap();
    cache.set_branch_head("vb", &head).unwrap();

    let cfg = MountConfig {
        id: "testid".into(),
        mount_point: state_dir.join("mnt"),
        // Unreachable on purpose: any lazy fetch should fail fast (connection
        // refused) rather than hit the network or hang.
        remote_url: "http://127.0.0.1:1".into(),
        owner: "oak".into(),
        repo: "myrepo".into(),
        base_branch: "main".into(),
        base_commit: head.as_str().to_string(),
        virtual_branch: "vb".into(),
    };

    Seed {
        _dir: dir,
        state_dir,
        cache,
        manifest,
        sizes,
        cfg,
    }
}

/// Construct a `MountCore` from a `Seed` on a throwaway multi-thread runtime.
/// Returns the runtime too so its `Handle` (stored in the core) stays valid for
/// the test's lifetime.
fn build(seed: Seed) -> (MountCore, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let core = MountCore::new(
        seed.cfg,
        seed.cache,
        &seed.manifest,
        &seed.sizes,
        None,
        rt.handle().clone(),
        seed.state_dir,
        SystemTime::now(),
    )
    .expect("MountCore::new should succeed");
    (core, rt)
}

fn names(entries: &[DirEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn builds_tree_from_manifest() {
    let seed = seed(
        &[("README.md", b"# hi\n"), ("src/lib.rs", b"fn main(){}")],
        &[],
    );
    let (core, _rt) = build(seed);

    let root = names(&core.readdir(ROOT_INODE).unwrap());
    assert!(root.contains(&"README.md".to_string()), "root: {root:?}");
    assert!(root.contains(&"src".to_string()), "root: {root:?}");

    let readme = core
        .lookup(ROOT_INODE, OsStr::new("README.md"))
        .expect("README.md should exist");
    assert_eq!(readme.kind, EntryKind::File);
    assert_eq!(readme.size, 5, "size of \"# hi\\n\"");

    // Intermediate directories are synthesized from the flat manifest path.
    let src = core
        .lookup(ROOT_INODE, OsStr::new("src"))
        .expect("src dir should exist");
    assert_eq!(src.kind, EntryKind::Dir);
    let src_children = names(&core.readdir(src.ino).unwrap());
    assert!(
        src_children.contains(&"lib.rs".to_string()),
        "src: {src_children:?}"
    );
}

#[test]
fn omits_overlay_deleted_file() {
    let s = seed(&[("keep.txt", b"k"), ("gone.txt", b"g")], &[]);
    let mut overlay = state::load_overlay_meta(&s.state_dir).unwrap();
    overlay.deletions.push("gone.txt".into());
    state::save_overlay_meta(&s.state_dir, &overlay).unwrap();

    let (core, _rt) = build(s);
    assert!(core.lookup(ROOT_INODE, OsStr::new("keep.txt")).is_some());
    assert!(
        core.lookup(ROOT_INODE, OsStr::new("gone.txt")).is_none(),
        "file deleted in the overlay should not appear in the tree"
    );
}

#[test]
fn reflects_overlay_rename() {
    let s = seed(&[("old.txt", b"x")], &[]);
    let mut overlay = state::load_overlay_meta(&s.state_dir).unwrap();
    overlay.renames.insert("old.txt".into(), "new.txt".into());
    state::save_overlay_meta(&s.state_dir, &overlay).unwrap();

    let (core, _rt) = build(s);
    assert!(
        core.lookup(ROOT_INODE, OsStr::new("old.txt")).is_none(),
        "the pre-rename path should be gone"
    );
    assert!(
        core.lookup(ROOT_INODE, OsStr::new("new.txt")).is_some(),
        "the post-rename path should be present"
    );
}

#[test]
fn layers_in_dirty_created_file() {
    let s = seed(&[("base.txt", b"b")], &[]);

    // Simulate a file the user created inside the mount: an overlay blob plus a
    // dirty entry pointing at it. It isn't in the manifest, so it must be
    // layered into the tree from the overlay alone.
    let overlay_file = state::overlay_filename_for("created.txt");
    std::fs::write(
        state::overlay_dir(&s.state_dir).join(&overlay_file),
        b"hello new",
    )
    .unwrap();
    let mut overlay = state::load_overlay_meta(&s.state_dir).unwrap();
    overlay.dirty.insert(
        "created.txt".into(),
        state::DirtyEntry {
            overlay_file,
            mode: "regular".into(),
            in_place: false,
        },
    );
    state::save_overlay_meta(&s.state_dir, &overlay).unwrap();

    let (core, _rt) = build(s);
    let created = core
        .lookup(ROOT_INODE, OsStr::new("created.txt"))
        .expect("dirty-created file should appear in the tree");
    assert_eq!(created.kind, EntryKind::File);
    assert_eq!(
        created.size, 9,
        "size taken from the overlay file (\"hello new\")"
    );
}

/// Regression for the mount-startup panic. `serve` constructs the `MountCore`
/// from inside an `async fn` — i.e. on a tokio worker thread. Hydrating the
/// ignore file uses a blocking `block_on`, which used to run on that worker
/// thread and panic ("Cannot start a runtime from within a runtime"). It now
/// runs on a dedicated scratch thread, so construction is safe from any
/// context. We include a `.gitignore` entry whose blob is *not* cached, which
/// is exactly what drives `load_mount_ignore` down the `block_on` path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn constructs_from_async_runtime_worker_without_panicking() {
    // Hash only — the blob is never stored, forcing the hydrate path.
    let phantom = Blob::new(b"target/\n".to_vec()).hash;
    let s = seed(&[("README.md", b"# hi\n")], &[(".gitignore", phantom)]);

    // Mirror `serve`: call `new` directly on this worker thread, handing it the
    // current runtime handle. On the pre-fix code this panics; it must not now.
    let core = MountCore::new(
        s.cfg,
        s.cache,
        &s.manifest,
        &s.sizes,
        None,
        tokio::runtime::Handle::current(),
        s.state_dir,
        SystemTime::now(),
    )
    .expect("construction from an async worker thread should not panic");

    // The unreachable remote just means the ignore file stays unhydrated; the
    // rest of the tree is built normally.
    assert!(core.lookup(ROOT_INODE, OsStr::new("README.md")).is_some());
}
