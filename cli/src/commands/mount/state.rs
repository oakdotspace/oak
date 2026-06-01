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
use std::sync::Mutex;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

/// Serializes read-modify-write of the mount index within one process so
/// `register_mount` and `unregister_mount` can't race against each other.
/// Cross-process races on the same index file are still possible — running
/// two `oak mount start` invocations literally simultaneously could lose a
/// registration — but that's a niche scenario and not what these tests
/// exercise.
static INDEX_LOCK: Mutex<()> = Mutex::new(());

/// One mount point's persistent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// Mount id (also the directory name under `~/.oak/mounts/`).
    pub id: String,
    /// Absolute path of the FUSE mount point.
    pub mount_point: PathBuf,
    /// Remote server URL (e.g. https://oakvcs.com).
    pub remote_url: String,
    /// Organization/owner segment.
    pub owner: String,
    /// Repo name.
    pub repo: String,
    /// Source branch the mount is anchored to.
    pub base_branch: String,
    /// Commit hash on the source branch at mount-creation time.
    pub base_commit: String,
    /// Virtual branch name (`<dest-slug>--<id8>`); only exists in the
    /// mount-local SQLite cache until pushed.
    pub virtual_branch: String,
    /// Active team slug (the `--team` flag at mount time). `None` if the
    /// mount isn't team-scoped. Resolved into `path_prefixes` below.
    #[serde(default)]
    pub team: Option<String>,
    /// Active project slug (the `--project` flag at mount time). `None`
    /// if the mount isn't project-scoped.
    #[serde(default)]
    pub project: Option<String>,
    /// Path prefixes that bound the mount's working tree, resolved from
    /// the team / project scope. Empty = whole-repo mount.
    #[serde(default)]
    pub path_prefixes: Vec<String>,
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let txt = serde_json::to_string_pretty(index)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(&path, txt)?;
    Ok(())
}

pub fn lookup_id_for(mount_point: &Path) -> Result<Option<String>> {
    let key = canonical_key(mount_point);
    Ok(load_index()?.mounts.get(&key).cloned())
}

pub fn register_mount(mount_point: &Path, id: &str) -> Result<()> {
    let _g = INDEX_LOCK.lock().unwrap();
    let mut idx = load_index()?;
    idx.mounts
        .insert(canonical_key(mount_point), id.to_string());
    save_index(&idx)
}

pub fn unregister_mount(mount_point: &Path) -> Result<()> {
    let _g = INDEX_LOCK.lock().unwrap();
    let mut idx = load_index()?;
    idx.mounts.remove(&canonical_key(mount_point));
    save_index(&idx)
}

pub fn save_config(state_dir: &Path, cfg: &MountConfig) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    let txt = toml::to_string_pretty(cfg)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(config_path(state_dir), txt)?;
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
    serde_json::from_str(&txt).map_err(|e| {
        OakError::Io(std::io::Error::other(format!(
            "invalid overlay metadata: {e}"
        )))
    })
}

pub fn save_overlay_meta(state_dir: &Path, meta: &OverlayMeta) -> Result<()> {
    let path = overlay_meta_path(state_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let txt = serde_json::to_string_pretty(meta)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(&path, txt)?;
    Ok(())
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let txt = serde_json::to_string_pretty(st)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(&path, txt)?;
    Ok(())
}

pub fn clear_sync_state(state_dir: &Path) -> Result<()> {
    let path = sync_state_path(state_dir);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Map an in-mount logical path (e.g. `src/main.rs`) to the overlay file name.
/// We flatten with `/` → `__` so the overlay dir stays one level deep, which
/// makes the rename/delete bookkeeping much simpler than mirroring the tree.
pub fn overlay_filename_for(path: &str) -> String {
    let mut s = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '/' => s.push_str("__"),
            '\\' => s.push_str("__"),
            _ => s.push(ch),
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
    fn overlay_filename_flattens_slashes() {
        assert_eq!(overlay_filename_for("foo.rs"), "foo.rs");
        assert_eq!(overlay_filename_for("src/foo.rs"), "src__foo.rs");
        assert_eq!(overlay_filename_for("a/b/c/file.txt"), "a__b__c__file.txt");
        assert_eq!(overlay_filename_for(""), "");
        // Backslash also flattened (defensive — we don't expect Windows
        // paths to flow into here, but the function shouldn't blow up).
        assert_eq!(overlay_filename_for("a\\b\\c"), "a__b__c");
    }

    #[test]
    fn config_roundtrip() {
        let dir = TempDir::new().unwrap();
        let cfg = MountConfig {
            id: "abc123".to_string(),
            mount_point: PathBuf::from("/tmp/mnt"),
            remote_url: "https://oakvcs.com".to_string(),
            owner: "ws".to_string(),
            repo: "myrepo".to_string(),
            base_branch: "main".to_string(),
            base_commit: "deadbeef".to_string(),
            virtual_branch: "main--mount-abc12345".to_string(),
            team: None,
            project: None,
            path_prefixes: Vec::new(),
        };
        save_config(dir.path(), &cfg).unwrap();
        let loaded = load_config(dir.path()).unwrap();
        assert_eq!(loaded.id, cfg.id);
        assert_eq!(loaded.mount_point, cfg.mount_point);
        assert_eq!(loaded.remote_url, cfg.remote_url);
        assert_eq!(loaded.virtual_branch, cfg.virtual_branch);
        assert!(loaded.team.is_none());
        assert!(loaded.project.is_none());
        assert!(loaded.path_prefixes.is_empty());
    }

    #[test]
    fn config_roundtrip_with_scope() {
        let dir = TempDir::new().unwrap();
        let cfg = MountConfig {
            id: "abc123".into(),
            mount_point: PathBuf::from("/tmp/mnt"),
            remote_url: "https://oakvcs.com".into(),
            owner: "ws".into(),
            repo: "myrepo".into(),
            base_branch: "main".into(),
            base_commit: "deadbeef".into(),
            virtual_branch: "main--mount-abc12345".into(),
            team: None,
            project: Some("payments".into()),
            path_prefixes: vec!["/payments/".to_string()],
        };
        save_config(dir.path(), &cfg).unwrap();
        let loaded = load_config(dir.path()).unwrap();
        assert_eq!(loaded.project.as_deref(), Some("payments"));
        assert_eq!(loaded.path_prefixes, vec!["/payments/".to_string()]);
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
}
