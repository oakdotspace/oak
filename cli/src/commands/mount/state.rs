//! On-disk layout and config for `oak mount`.
//!
//! Each mount has its own state directory at `~/.oak/mounts/<id>/` containing:
//! - `config.toml`: organization, repo, remote, virtual branch, base commit
//! - `cache.db`: a normal `SqliteRepository` used as a lazy blob cache
//! - `overlay/`: real on-disk files for paths that have been written through
//!   the FUSE layer (the "dirty" set)
//! - `overlay-meta.json`: per-path overlay metadata (deletions, mode, renames)
//!
//! A registry at `~/.oak/mounts/index.json` maps mount-point paths to ids so
//! `oak mount status|commit|push` can find the right state from a destination
//! directory alone.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use oak_core::{Hash, OakError, Repository, Result, SqliteRepository};
use serde::{Deserialize, Serialize};

/// RAII guard over an advisory cross-process file lock. The lock is released
/// when the guard drops — and, because it's an OS-level lock on the open file
/// (not a marker file), also when the holding process dies, so a crash never
/// leaves a stale lock behind.
///
/// Two threads in one process exclude each other too (each acquire opens its
/// own file description), so this replaces the old in-process index mutex.
pub struct StateLock {
    _file: fs::File,
}

/// Acquire an exclusive advisory lock on `path`, blocking until it's free.
///
/// Unix uses `flock`. Windows has no flock; locking there is best-effort
/// (no exclusion), which matches the previous behavior on that platform —
/// kept minimal and conservative rather than risking an untestable spin-lock.
pub fn acquire_lock(path: &Path) -> Result<StateLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock on a valid owned fd; blocks until the lock is granted.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(OakError::Io(std::io::Error::last_os_error()));
        }
    }
    Ok(StateLock { _file: file })
}

/// One mount point's persistent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// Mount id (also the directory name under `~/.oak/mounts/`).
    pub id: String,
    /// Absolute path of the FUSE mount point.
    pub mount_point: PathBuf,
    /// Remote server URL (e.g. https://oak.space).
    pub remote_url: String,
    /// Organization/owner segment.
    pub owner: String,
    /// Repo name.
    pub repo: String,
    /// Source branch the mount is anchored to.
    pub base_branch: String,
    /// Commit hash on the source branch at mount-creation time.
    pub base_commit: String,
    /// Virtual branch name (`<task-slug>--<id8>`); only exists in the
    /// mount-local SQLite cache until pushed.
    pub virtual_branch: String,
}

/// Registry index mapping mount-point absolute path → mount id.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MountIndex {
    pub mounts: HashMap<String, String>,
}

/// Per-path metadata for the overlay (changes that haven't been committed).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OverlayMeta {
    /// Paths the user has deleted (but that exist in the base manifest).
    #[serde(default)]
    pub deletions: Vec<String>,
    /// Paths created or modified through the FUSE layer. Each maps to the
    /// overlay filename (relative to `overlay/`) holding its content.
    #[serde(default)]
    pub dirty: HashMap<String, DirtyEntry>,
    /// Renames the user has performed: old path → new path.
    #[serde(default)]
    pub renames: HashMap<String, String>,
}

/// State recorded while a mount's `oak pull` is mid conflict-resolution.
///
/// Written by `oak pull` (when the parent-merge produces conflicts) and read
/// by `oak pull --continue` (finalize) / `oak pull --abort` (discard). Lives at
/// `<state_dir>/sync-state.json`. Its presence is what blocks a plain
/// `oak commit` / `oak push` from running over a half-resolved merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSyncState {
    /// Parent branch the pull merged from (the trunk today).
    pub parent_name: String,
    /// Parent branch HEAD at pull time. Stamped as the finalized sync commit's
    /// `merge_parent_hash` so later merge-base walks find a recent LCA.
    pub parent_head: String,
    /// Hash of the fully-merged manifest, stored in the cache db. Clean
    /// (non-conflicting) parent changes live only here — they never touch the
    /// overlay — and `--continue` layers the user's resolved files on top.
    pub merged_manifest: String,
    /// Paths with conflict markers written into the overlay for the user to
    /// resolve. `--continue` refuses to finalize while any still carry markers.
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtyEntry {
    /// Where the dirty content lives. Interpretation depends on `in_place`:
    /// when `false` (FUSE backend), this is a flat filename inside
    /// `<state_dir>/overlay/`. When `true` (ProjFS backend), this field is
    /// unused and the content is read from `<mount_point>/<path>` directly,
    /// because ProjFS persists modifications in-place on disk and copying
    /// them into a separate overlay would double-write large binary assets.
    pub overlay_file: String,
    /// File mode at write time.
    pub mode: String,
    /// True if the dirty content lives in the mount tree itself rather than
    /// the overlay dir. Set by the ProjFS notification handler; FUSE writes
    /// always materialize into the flat overlay first and leave this `false`.
    #[serde(default)]
    pub in_place: bool,
}

thread_local! {
    /// Per-thread override for the mounts root. Takes precedence over the
    /// `OAK_MOUNTS_ROOT` env var and the `~/.oak/mounts` default.
    ///
    /// This exists so tests can isolate their on-disk mount state without
    /// mutating the process-global environment: `cargo test` runs each test on
    /// its own thread, so a per-thread override gives each test a private root
    /// and eliminates the shared-index race that a single process-global root
    /// (env var) would cause under multi-threaded execution. Production code
    /// never sets this, so the env / home resolution below is unchanged for
    /// real CLI usage.
    static MOUNTS_ROOT_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Override the mounts root for the current thread. Intended for tests; pass a
/// distinct temp dir per test so their state never collides. Production callers
/// should not use this — they rely on the env / home resolution in
/// [`mounts_root`].
pub fn set_mounts_root(root: PathBuf) {
    MOUNTS_ROOT_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(root));
}

/// Root for all mount state. Resolution order: a per-thread override (set via
/// [`set_mounts_root`], used by tests), then the `OAK_MOUNTS_ROOT` env var,
/// then the `~/.oak/mounts/` default.
pub fn mounts_root() -> Result<PathBuf> {
    if let Some(root) = MOUNTS_ROOT_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return Ok(root);
    }
    if let Ok(root) = std::env::var("OAK_MOUNTS_ROOT") {
        if !root.is_empty() {
            return Ok(PathBuf::from(root));
        }
    }
    let home = dirs::home_dir()
        .ok_or_else(|| OakError::Io(std::io::Error::other("Could not determine home directory")))?;
    Ok(home.join(".oak").join("mounts"))
}

pub fn index_path() -> Result<PathBuf> {
    Ok(mounts_root()?.join("index.json"))
}

pub fn state_dir_for(id: &str) -> Result<PathBuf> {
    Ok(mounts_root()?.join(id))
}

pub fn config_path(state_dir: &Path) -> PathBuf {
    state_dir.join("config.toml")
}

pub fn cache_db_path(state_dir: &Path) -> PathBuf {
    state_dir.join("cache.db")
}

pub fn overlay_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("overlay")
}

pub fn overlay_meta_path(state_dir: &Path) -> PathBuf {
    state_dir.join("overlay-meta.json")
}

pub fn sync_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("sync-state.json")
}

/// Lock file serializing read-modify-write of `index.json` across processes.
fn index_lock_path() -> Result<PathBuf> {
    Ok(mounts_root()?.join("index.lock"))
}

/// Lock file serializing mutation of `overlay-meta.json` between the mount
/// daemon (`persist_overlay`) and out-of-band CLI processes (`oak commit`
/// clearing the overlay).
pub fn overlay_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("overlay-meta.lock")
}

/// Take the overlay-meta mutation lock for `state_dir`. Both the daemon and
/// any CLI process that rewrites `overlay-meta.json` must hold this while
/// mutating, so neither can clobber the other's write mid-flight.
pub fn lock_overlay_meta(state_dir: &Path) -> Result<StateLock> {
    acquire_lock(&overlay_lock_path(state_dir))
}

/// Pid of the detached daemon serving this mount, recorded at spawn so
/// teardown can terminate it instead of leaking one process per mount.
pub fn daemon_pid_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.pid")
}

pub fn save_daemon_pid(state_dir: &Path, pid: u32) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    fs::write(daemon_pid_path(state_dir), pid.to_string())?;
    Ok(())
}

pub fn load_daemon_pid(state_dir: &Path) -> Option<u32> {
    fs::read_to_string(daemon_pid_path(state_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether `pid` is a live process. Used to tell a running mount daemon from
/// a stale registration left behind by a crash or reboot.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) probes for existence without signaling. EPERM still means
    // the process exists (it just isn't ours).
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Probe process existence on Windows: open it for query and check whether it
/// still reports `STILL_ACTIVE`. A process that legitimately exited with code
/// 259 reads as alive — a rare, harmless false-positive that keeps teardown
/// conservative. A pid we can't open (gone, or access-denied) reads as dead.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, FALSE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // STILL_ACTIVE (STATUS_PENDING, 259): GetExitCodeProcess reports this for a
    // process that hasn't exited yet.
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: we close every handle we open and pass a valid out-pointer.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) else {
            return false;
        };
        let mut code: u32 = 0;
        let alive = GetExitCodeProcess(handle, &mut code).is_ok() && code == STILL_ACTIVE;
        let _ = CloseHandle(handle);
        alive
    }
}

/// No cheap existence probe is wired up on this platform; assume a recorded
/// pid is alive so teardown stays conservative (refuses rather than deleting
/// state out from under a possibly-running daemon).
#[cfg(not(any(unix, windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

/// Virtual-branch head as of the last successful `oak push` from this mount.
/// Lets teardown distinguish committed-but-unpushed work (which would be
/// destroyed with the state dir) from work that's safely on the server.
pub fn pushed_head_path(state_dir: &Path) -> PathBuf {
    state_dir.join("pushed-head")
}

pub fn save_pushed_head(state_dir: &Path, head: &str) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    crate::atomic_file::write_atomic(&pushed_head_path(state_dir), head)?;
    Ok(())
}

pub fn load_pushed_head(state_dir: &Path) -> Option<String> {
    let txt = fs::read_to_string(pushed_head_path(state_dir)).ok()?;
    let head = txt.trim().to_string();
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

/// True when the overlay has no uncommitted edits (missing metadata counts as clean).
pub fn overlay_is_clean(state_dir: &Path) -> bool {
    let path = overlay_meta_path(state_dir);
    if !path.exists() {
        return true;
    }
    load_overlay_meta(state_dir)
        .map(|overlay| {
            overlay.dirty.is_empty() && overlay.deletions.is_empty() && overlay.renames.is_empty()
        })
        .unwrap_or(true)
}

/// Commits on the mount's virtual branch not yet recorded in `pushed-head`.
///
/// Lossy convenience wrapper for display/non-safety callers: an unreadable
/// state collapses to 0. **Teardown-safety decisions must NOT use this** —
/// "couldn't read the cache" and "definitely zero unpushed" both come back as
/// 0, which is exactly how a transiently-locked cache.db led to unpushed work
/// being deleted. Use [`unpushed_commit_count_checked`] there.
pub fn unpushed_commit_count(state_dir: &Path) -> usize {
    unpushed_commit_count_checked(state_dir).unwrap_or(0)
}

/// Teardown-safe unpushed-commit count.
///
/// `Some(n)` = definitively `n` unpushed commits. `Some(0)` includes the
/// half-created mount that never got a `cache.db` — there's nothing to lose, so
/// it's safe to remove. `None` = the cache exists but its state couldn't be
/// read (locked by another process, corrupt, config gone): we genuinely don't
/// know, so teardown must treat it as "may hold unpushed work" and refuse
/// without `--force`, rather than failing open and destroying the only copy.
pub fn unpushed_commit_count_checked(state_dir: &Path) -> Option<usize> {
    let cache_path = cache_db_path(state_dir);
    if !cache_path.exists() {
        // No cache.db → no virtual-branch commits could exist. Nothing to lose.
        return Some(0);
    }
    // The cache exists, so any read failure below is "unknown", not "empty".
    let cfg = load_config(state_dir).ok()?;
    let cache = SqliteRepository::open_relaxed(&cache_path).ok()?;

    let count = cache.count_commits_for_branch(&cfg.virtual_branch).ok()?;
    if count == 0 {
        return Some(0);
    }

    if let Some(pushed) = load_pushed_head(state_dir) {
        if cache
            .get_branch_head(&cfg.virtual_branch)
            .ok()
            .flatten()
            .is_some_and(|head| head.as_str() == pushed)
        {
            return Some(0);
        }
    }

    let since = load_pushed_head(state_dir).map(Hash);
    let commits = cache
        .get_commits_since(&cfg.virtual_branch, since.as_ref())
        .ok()?;
    Some(commits.len())
}

/// True when overlay metadata is present but can't be parsed — its dirty/clean
/// state is unknown. Teardown must treat this as "not provably clean" rather
/// than the `unwrap_or_default()` fail-open that reports a corrupt overlay as
/// clean.
pub fn overlay_state_unreadable(state_dir: &Path) -> bool {
    overlay_meta_path(state_dir).exists() && load_overlay_meta(state_dir).is_err()
}

/// Canonicalize a mount-point path for use as a registry key. Falls back to
/// the absolute path if canonicalization fails (e.g. mount point doesn't
/// exist yet).
pub fn canonical_key(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

pub fn load_index() -> Result<MountIndex> {
    let path = index_path()?;
    if !path.exists() {
        return Ok(MountIndex::default());
    }
    let txt = fs::read_to_string(&path)?;
    serde_json::from_str(&txt)
        .map_err(|e| OakError::Io(std::io::Error::other(format!("invalid mount index: {e}"))))
}

pub fn save_index(index: &MountIndex) -> Result<()> {
    let path = index_path()?;
    let txt = serde_json::to_string_pretty(index)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    crate::atomic_file::write_atomic(&path, txt)
}

pub fn lookup_id_for(mount_point: &Path) -> Result<Option<String>> {
    let key = canonical_key(mount_point);
    Ok(load_index()?.mounts.get(&key).cloned())
}

pub fn register_mount(mount_point: &Path, id: &str) -> Result<()> {
    let _g = acquire_lock(&index_lock_path()?)?;
    let mut idx = load_index()?;
    idx.mounts
        .insert(canonical_key(mount_point), id.to_string());
    save_index(&idx)
}

pub fn unregister_mount(mount_point: &Path) -> Result<()> {
    let _g = acquire_lock(&index_lock_path()?)?;
    let mut idx = load_index()?;
    idx.mounts.remove(&canonical_key(mount_point));
    save_index(&idx)
}

pub fn save_config(state_dir: &Path, cfg: &MountConfig) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    let txt = toml::to_string_pretty(cfg)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    crate::atomic_file::write_atomic(&config_path(state_dir), txt)?;
    Ok(())
}

pub fn load_config(state_dir: &Path) -> Result<MountConfig> {
    let txt = fs::read_to_string(config_path(state_dir))?;
    toml::from_str(&txt)
        .map_err(|e| OakError::Io(std::io::Error::other(format!("invalid mount config: {e}"))))
}

pub fn load_overlay_meta(state_dir: &Path) -> Result<OverlayMeta> {
    let path = overlay_meta_path(state_dir);
    if !path.exists() {
        return Ok(OverlayMeta::default());
    }
    let txt = fs::read_to_string(&path)?;
    let mut meta: OverlayMeta = serde_json::from_str(&txt).map_err(|e| {
        OakError::Io(std::io::Error::other(format!(
            "invalid overlay metadata: {e}"
        )))
    })?;
    // Drop OS-generated metadata (.fseventsd, .DS_Store, …) that the mount may
    // have recorded before reaching `is_system_metadata_path`, or that an older
    // build persisted. The OS recreates these on demand, so it's safe to forget
    // them rather than surface them in status / commit / push.
    meta.dirty
        .retain(|p, _| !oak_core::is_system_metadata_path(p));
    meta.deletions
        .retain(|p| !oak_core::is_system_metadata_path(p));
    meta.renames.retain(|old, new| {
        !oak_core::is_system_metadata_path(old) && !oak_core::is_system_metadata_path(new)
    });
    Ok(meta)
}

pub fn save_overlay_meta(state_dir: &Path, meta: &OverlayMeta) -> Result<()> {
    let path = overlay_meta_path(state_dir);
    let txt = serde_json::to_string_pretty(meta)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    // Atomic write: the daemon and out-of-band CLI processes read this file
    // concurrently, and a torn read would parse-fail and drop overlay state.
    crate::atomic_file::write_atomic(&path, txt)
}

/// Load the in-progress pull sync state, or `None` if no pull is mid-resolution.
pub fn load_sync_state(state_dir: &Path) -> Result<Option<MountSyncState>> {
    let path = sync_state_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let txt = fs::read_to_string(&path)?;
    serde_json::from_str(&txt).map(Some).map_err(|e| {
        OakError::Io(std::io::Error::other(format!(
            "invalid mount sync state: {e}"
        )))
    })
}

pub fn save_sync_state(state_dir: &Path, st: &MountSyncState) -> Result<()> {
    let path = sync_state_path(state_dir);
    let txt = serde_json::to_string_pretty(st)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    crate::atomic_file::write_atomic(&path, txt)
}

pub fn clear_sync_state(state_dir: &Path) -> Result<()> {
    let path = sync_state_path(state_dir);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Map an in-mount logical path (e.g. `src/main.rs`) to the overlay file name.
/// Keep the overlay dir one level deep, but make the mapping injective: a plain
/// slash-flattening scheme aliases paths such as `a/b` and `a__b`.
pub fn overlay_filename_for(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(path.len() + 2);
    s.push_str("p_");
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                s.push(char::from(b));
            }
            _ => {
                s.push('%');
                s.push(char::from(HEX[(b >> 4) as usize]));
                s.push(char::from(HEX[(b & 0x0f) as usize]));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn overlay_filename_is_injective_for_separator_like_paths() {
        assert_eq!(overlay_filename_for("foo.rs"), "p_foo.rs");
        assert_eq!(overlay_filename_for("src/foo.rs"), "p_src%2Ffoo.rs");
        assert_eq!(
            overlay_filename_for("a/b/c/file.txt"),
            "p_a%2Fb%2Fc%2Ffile.txt"
        );
        assert_eq!(overlay_filename_for(""), "p_");
        // Backslash is escaped too (defensive — we don't expect Windows paths
        // to flow into here, but it must not alias slash or literal "__").
        assert_eq!(overlay_filename_for("a\\b\\c"), "p_a%5Cb%5Cc");
        assert_ne!(overlay_filename_for("a/b"), overlay_filename_for("a__b"));
    }

    #[test]
    fn config_roundtrip() {
        let dir = TempDir::new().unwrap();
        let cfg = MountConfig {
            id: "abc123".to_string(),
            mount_point: PathBuf::from("/tmp/mnt"),
            remote_url: "https://oak.space".to_string(),
            owner: "ws".to_string(),
            repo: "myrepo".to_string(),
            base_branch: "main".to_string(),
            base_commit: "deadbeef".to_string(),
            virtual_branch: "main--mount-abc12345".to_string(),
        };
        save_config(dir.path(), &cfg).unwrap();
        let loaded = load_config(dir.path()).unwrap();
        assert_eq!(loaded.id, cfg.id);
        assert_eq!(loaded.mount_point, cfg.mount_point);
        assert_eq!(loaded.remote_url, cfg.remote_url);
        assert_eq!(loaded.virtual_branch, cfg.virtual_branch);
    }

    #[test]
    fn overlay_meta_roundtrip() {
        let dir = TempDir::new().unwrap();
        let mut meta = OverlayMeta::default();
        meta.deletions.push("removed.txt".into());
        meta.dirty.insert(
            "src/foo.rs".into(),
            DirtyEntry {
                overlay_file: "src__foo.rs".into(),
                mode: "regular".into(),
                in_place: false,
            },
        );
        meta.renames.insert("old.txt".into(), "new.txt".into());

        save_overlay_meta(dir.path(), &meta).unwrap();
        let loaded = load_overlay_meta(dir.path()).unwrap();
        assert_eq!(loaded.deletions, meta.deletions);
        assert_eq!(loaded.dirty.len(), 1);
        assert_eq!(
            loaded.dirty.get("src/foo.rs").unwrap().overlay_file,
            "src__foo.rs"
        );
        assert_eq!(
            loaded.renames.get("old.txt").map(String::as_str),
            Some("new.txt")
        );
    }

    #[test]
    fn overlay_meta_missing_file_is_default() {
        let dir = TempDir::new().unwrap();
        let loaded = load_overlay_meta(dir.path()).unwrap();
        assert!(loaded.deletions.is_empty());
        assert!(loaded.dirty.is_empty());
        assert!(loaded.renames.is_empty());
    }

    #[test]
    fn sync_state_roundtrip_uses_final_path_without_temp_leftovers() {
        let dir = TempDir::new().unwrap();
        let state = MountSyncState {
            parent_name: "main".into(),
            parent_head: "parent-head".into(),
            merged_manifest: "merged-manifest".into(),
            conflicts: vec!["src/lib.rs".into()],
        };

        save_sync_state(dir.path(), &state).unwrap();
        let loaded = load_sync_state(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.parent_name, state.parent_name);
        assert_eq!(loaded.parent_head, state.parent_head);
        assert_eq!(loaded.merged_manifest, state.merged_manifest);
        assert_eq!(loaded.conflicts, state.conflicts);

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "sync-state atomic write should not leave temp files: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn acquire_lock_excludes_other_holders() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.lock");

        let guard = acquire_lock(&path).unwrap();
        let acquired = Arc::new(AtomicBool::new(false));
        let flag = acquired.clone();
        let p = path.clone();
        let waiter = std::thread::spawn(move || {
            let _g = acquire_lock(&p).unwrap();
            flag.store(true, Ordering::SeqCst);
        });

        // The second holder opens its own file description, so it must block
        // until the first guard drops.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !acquired.load(Ordering::SeqCst),
            "second acquire should block while the lock is held"
        );

        drop(guard);
        waiter.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
    }

    #[test]
    fn daemon_pid_roundtrip() {
        let dir = TempDir::new().unwrap();
        assert_eq!(load_daemon_pid(dir.path()), None);
        save_daemon_pid(dir.path(), 4242).unwrap();
        assert_eq!(load_daemon_pid(dir.path()), Some(4242));
    }

    #[test]
    fn pushed_head_roundtrip() {
        let dir = TempDir::new().unwrap();
        assert_eq!(load_pushed_head(dir.path()), None);
        save_pushed_head(dir.path(), "abc123").unwrap();
        assert_eq!(load_pushed_head(dir.path()), Some("abc123".to_string()));
    }

    #[test]
    fn overlay_is_clean_when_metadata_missing() {
        let dir = TempDir::new().unwrap();
        assert!(overlay_is_clean(dir.path()));
    }

    #[test]
    fn unpushed_commit_count_empty_without_cache() {
        let dir = TempDir::new().unwrap();
        assert_eq!(unpushed_commit_count(dir.path()), 0);
    }

    #[test]
    fn checked_count_is_zero_when_no_cache_db() {
        // A half-created mount with no cache.db has nothing to lose: safe to
        // tear down. This must be `Some(0)`, not `None`.
        let dir = TempDir::new().unwrap();
        assert_eq!(unpushed_commit_count_checked(dir.path()), Some(0));
    }

    #[test]
    fn checked_count_is_unknown_when_cache_db_unreadable() {
        // Regression: an unreadable cache.db must come back as `None` (unknown),
        // not silently as 0 — otherwise teardown deletes possibly-unpushed work.
        let dir = TempDir::new().unwrap();
        // Write garbage where the cache db belongs so SqliteRepository::open
        // fails, while a config exists so we get past that step.
        save_config(
            dir.path(),
            &MountConfig {
                id: "abc12345".to_string(),
                mount_point: dir.path().join("mnt"),
                remote_url: "https://oak.space".to_string(),
                owner: "oak".to_string(),
                repo: "oak".to_string(),
                base_branch: "main".to_string(),
                base_commit: "0".repeat(64),
                virtual_branch: "feature--mount-abc12345".to_string(),
            },
        )
        .unwrap();
        fs::write(cache_db_path(dir.path()), b"not a sqlite database").unwrap();
        assert_eq!(unpushed_commit_count_checked(dir.path()), None);
        // The lossy wrapper still collapses to 0 (display-only behavior).
        assert_eq!(unpushed_commit_count(dir.path()), 0);
    }

    #[test]
    fn overlay_state_unreadable_detects_corrupt_meta() {
        let dir = TempDir::new().unwrap();
        // No file → readable (clean).
        assert!(!overlay_state_unreadable(dir.path()));
        // Corrupt JSON → unreadable.
        fs::write(overlay_meta_path(dir.path()), b"{ not json").unwrap();
        assert!(overlay_state_unreadable(dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_detects_self_and_dead() {
        assert!(pid_alive(std::process::id()));
        // A pid far above any plausible live process. If this is ever flaky,
        // it found a real process and the assertion is simply skipped by luck
        // of the draw — but pid_max on macOS/Linux keeps real pids well below.
        assert!(!pid_alive(99_999_999));
    }
}
