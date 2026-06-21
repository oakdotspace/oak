//! Persistent per-repo head cache shared across mounts of the same repo.
//!
//! The agent workflow remounts the same repo for every follow-up task
//! (mount → work → end → remount), and `oak mount end` deletes the whole
//! per-mount state dir — so every remount used to re-fetch the head commit,
//! manifest, and blob metadata from the server even when the repo head was
//! unchanged. This module keeps a small per-repo store under
//! `<mounts_root>/repo-cache/<remote>/<owner>/<repo>/` that survives
//! teardown:
//!
//! - `cache.db`: a `SqliteRepository` holding the head commit, its manifest
//!   trees, and per-blob chunk refs (no blob content).
//! - `sizes-<commit>.json`: the exact blob-hash → size map served by
//!   `/blobs/info` for that head, written only after a successful fetch.
//! - `lock`: an flock serializing all access across processes.
//!
//! Correctness: everything is keyed strictly by the head *commit hash*
//! returned by the live repo-head request. Commits, trees, and chunk refs are
//! content-addressed — the same hash always names the same bytes — so a hit
//! can never serve stale content; a changed head simply misses. As a final
//! guard, [`try_seed`] re-derives the manifest's root tree hash from the
//! cached entries and refuses the fast path unless it equals the commit's
//! recorded `manifest_hash`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use oak_core::{Branch, BranchStatus, Hash, OakError, Repository, Result, SqliteRepository};

use super::state;

/// Flatten an arbitrary string (remote URL, owner, repo) into a single safe
/// path component, mirroring `spawn::log_path_for`'s slug scheme.
fn sanitize_component(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if slug.is_empty() {
        "-".to_string()
    } else {
        slug
    }
}

/// Directory holding the shared cache for one repo. Lives next to the
/// per-mount state dirs (whose names are uuids, so `repo-cache` can't
/// collide) and is never touched by `oak mount end` / `oak space clean`.
pub fn repo_cache_dir(remote_url: &str, owner: &str, repo: &str) -> Result<PathBuf> {
    Ok(state::mounts_root()?
        .join("repo-cache")
        .join(sanitize_component(remote_url))
        .join(sanitize_component(owner))
        .join(sanitize_component(repo)))
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join("cache.db")
}

fn sizes_path(dir: &Path, head: &Hash) -> PathBuf {
    dir.join(format!("sizes-{}.json", head.as_str()))
}

fn lock(dir: &Path) -> Result<state::StateLock> {
    state::acquire_lock(&dir.join("lock"))
}

/// How many per-head sizes files to keep per repo. Each is small (one JSON
/// row per unique blob), but heads accumulate over the repo's life; keep the
/// most recent few so the cache stays bounded without losing the heads that
/// concurrent mounts might still be racing on.
const KEEP_SIZES_FILES: usize = 8;

/// A branch row matching the cached commit's `branch_name`, so the commit's
/// FK target exists even if the seeded db is later opened with enforcement on.
fn branch_row_for(commit: &oak_core::Commit) -> Branch {
    Branch {
        name: commit.branch_name.clone(),
        description: None,
        parent_branch: None,
        status: BranchStatus::Open,
        close_reason: None,
        created_at: Utc::now(),
    }
}

/// Unique blob hashes referenced by a manifest.
fn unique_blob_hashes(manifest: &oak_core::Manifest) -> HashSet<&str> {
    manifest
        .entries
        .iter()
        .map(|e| e.blob_hash.as_str())
        .collect()
}

/// Try to seed a fresh mount cache for `head` entirely from the repo cache.
///
/// On a hit, stores the head commit, its manifest trees, and every cached
/// blob chunk-ref into `cache`, and returns the blob-size map the original
/// fetch recorded — the caller can then skip both the `commits/info` and
/// `blobs/info` round trips. Returns `Ok(None)` on any miss or integrity
/// doubt (unknown head, missing manifest, incomplete sizes, root-hash
/// mismatch); the caller falls back to the network path.
pub fn try_seed(
    cache: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo: &str,
    head: &Hash,
) -> Result<Option<HashMap<String, u64>>> {
    let dir = repo_cache_dir(remote_url, owner, repo)?;
    if !db_path(&dir).exists() {
        return Ok(None);
    }
    let _lock = lock(&dir)?;
    let shared = SqliteRepository::open_relaxed(&db_path(&dir))?;

    let Some(commit) = shared.get_commit(head)? else {
        return Ok(None);
    };
    let Some(manifest) = shared.get_manifest(&commit.manifest_hash)? else {
        return Ok(None);
    };
    let sizes: HashMap<String, u64> = match fs::read_to_string(sizes_path(&dir, head))
        .ok()
        .and_then(|txt| serde_json::from_str(&txt).ok())
    {
        Some(sizes) => sizes,
        None => return Ok(None),
    };
    // The sizes file is written only after a successful full fetch, but stay
    // strict: every blob the manifest references must have a recorded size,
    // or the FUSE layer would stat files as empty.
    let unique = unique_blob_hashes(&manifest);
    if unique.iter().any(|h| !sizes.contains_key(*h)) {
        return Ok(None);
    }

    cache.store_branch(&branch_row_for(&commit))?;
    // Re-derive the content-addressed root hash from the cached entries; a
    // mismatch means the cached manifest doesn't reproduce the commit's
    // recorded tree, so refuse the fast path rather than mount wrong content.
    let rebuilt = cache.put_manifest(manifest.entries.clone())?;
    if rebuilt.as_str() != commit.manifest_hash.as_str() {
        return Ok(None);
    }
    cache.store_commit(&commit)?;
    for h in &unique {
        let hash = Hash((*h).to_string());
        if let Some(chunks) = shared.get_blob_chunks(&hash)? {
            if !chunks.is_empty() {
                cache.store_blob_chunks(&hash, &chunks)?;
            }
        }
    }
    Ok(Some(sizes))
}

/// Record a freshly fetched head into the repo cache (best-effort; callers
/// log and continue on error). Copies the commit, manifest trees, and chunk
/// refs out of the mount's cache, then writes the sizes file last — its
/// presence is what marks the head as fully cached.
pub fn populate(
    cache: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo: &str,
    head: &Hash,
    sizes: &HashMap<String, u64>,
) -> Result<()> {
    let dir = repo_cache_dir(remote_url, owner, repo)?;
    fs::create_dir_all(&dir)?;
    let _lock = lock(&dir)?;
    let shared = SqliteRepository::open_relaxed(&db_path(&dir))?;

    let Some(commit) = cache.get_commit(head)? else {
        return Ok(());
    };
    let Some(manifest) = cache.get_manifest(&commit.manifest_hash)? else {
        return Ok(());
    };

    shared.store_branch(&branch_row_for(&commit))?;
    let rebuilt = shared.put_manifest(manifest.entries.clone())?;
    if rebuilt.as_str() != commit.manifest_hash.as_str() {
        // Shouldn't happen (the entries came straight from this manifest);
        // leave the head unrecorded rather than cache something we couldn't
        // reproduce.
        return Ok(());
    }
    // `store_commit` is INSERT OR IGNORE on the commit row but a plain INSERT
    // on its file rows, so only store a commit the cache hasn't seen.
    if shared.get_commit(head)?.is_none() {
        shared.store_commit(&commit)?;
    }
    for h in &unique_blob_hashes(&manifest) {
        let hash = Hash((*h).to_string());
        if shared.get_blob_chunks(&hash)?.is_some() {
            continue;
        }
        if let Some(chunks) = cache.get_blob_chunks(&hash)? {
            if !chunks.is_empty() {
                shared.store_blob_chunks(&hash, &chunks)?;
            }
        }
    }

    // Write-then-rename so a concurrent `try_seed` never reads a torn file.
    let path = sizes_path(&dir, head);
    let txt = serde_json::to_string(sizes)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, txt)?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(OakError::Io(e));
    }
    prune_sizes_files(&dir, head);
    Ok(())
}

/// Keep the cache bounded: drop the oldest sizes files beyond
/// [`KEEP_SIZES_FILES`], never the one just written. Best-effort.
fn prune_sizes_files(dir: &Path, current: &Hash) {
    let current_name = format!("sizes-{}.json", current.as_str());
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("sizes-") && name.ends_with(".json") && name != current_name
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    if files.len() < KEEP_SIZES_FILES {
        return;
    }
    files.sort_by_key(|(mtime, _)| *mtime);
    let excess = files.len() + 1 - KEEP_SIZES_FILES;
    for (_, path) in files.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{ChunkInfo, Commit, FileMode, Manifest, ManifestEntry};
    use tempfile::TempDir;

    // Tree serialization rejects non-hex hashes, so use valid 64-char hex.
    fn hex_hash(seed: char) -> String {
        std::iter::repeat_n(seed, 64).collect()
    }

    fn entry(path: &str, blob: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            blob_hash: Hash(blob.into()),
            mode: FileMode::Regular,
        }
    }

    /// Build a mount-style cache db holding a commit + manifest + chunk refs,
    /// the way `serve()`'s fetch phase leaves it.
    fn seed_source_cache(dir: &Path) -> (SqliteRepository, Commit, HashMap<String, u64>) {
        let cache = SqliteRepository::open_relaxed(&dir.join("cache.db")).unwrap();
        let entries = vec![
            entry("a.txt", &hex_hash('a')),
            entry("src/b.rs", &hex_hash('b')),
        ];
        let manifest_hash = cache.put_manifest(entries.clone()).unwrap();
        let commit = Commit {
            hash: Hash(hex_hash('d')),
            branch_name: "main".into(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash,
            author: "test".into(),
            message: None,
            timestamp: Utc::now(),
            files: Vec::new(),
        };
        cache.store_branch(&branch_row_for(&commit)).unwrap();
        cache.store_commit(&commit).unwrap();
        cache
            .store_blob_chunks(
                &Hash(hex_hash('a')),
                &[ChunkInfo {
                    hash: Hash(hex_hash('c')),
                    offset: 0,
                    length: 3,
                }],
            )
            .unwrap();
        let sizes: HashMap<String, u64> = [(hex_hash('a'), 3u64), (hex_hash('b'), 9u64)]
            .into_iter()
            .collect();
        (cache, commit, sizes)
    }

    #[test]
    fn populate_then_seed_roundtrip() {
        let root = TempDir::new().unwrap();
        state::set_mounts_root(root.path().to_path_buf());

        let src_dir = TempDir::new().unwrap();
        let (src, commit, sizes) = seed_source_cache(src_dir.path());
        populate(&src, "https://x", "o", "r", &commit.hash, &sizes).unwrap();

        let dst_dir = TempDir::new().unwrap();
        let dst = SqliteRepository::open_relaxed(&dst_dir.path().join("cache.db")).unwrap();
        let seeded = try_seed(&dst, "https://x", "o", "r", &commit.hash)
            .unwrap()
            .expect("cached head should hit");
        assert_eq!(seeded, sizes);

        let got = dst
            .get_commit(&commit.hash)
            .unwrap()
            .expect("commit seeded");
        assert_eq!(got.manifest_hash.as_str(), commit.manifest_hash.as_str());
        let manifest: Manifest = dst
            .get_manifest(&commit.manifest_hash)
            .unwrap()
            .expect("manifest seeded");
        assert_eq!(manifest.entries.len(), 2);
        let chunks = dst
            .get_blob_chunks(&Hash(hex_hash('a')))
            .unwrap()
            .expect("chunk refs seeded");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].hash.as_str(), hex_hash('c'));
    }

    #[test]
    fn seed_misses_unknown_head() {
        let root = TempDir::new().unwrap();
        state::set_mounts_root(root.path().to_path_buf());

        let src_dir = TempDir::new().unwrap();
        let (src, commit, sizes) = seed_source_cache(src_dir.path());
        populate(&src, "https://x", "o", "r", &commit.hash, &sizes).unwrap();

        let dst_dir = TempDir::new().unwrap();
        let dst = SqliteRepository::open_relaxed(&dst_dir.path().join("cache.db")).unwrap();
        // Different head hash → strict miss, even though the repo dir exists.
        let seeded = try_seed(&dst, "https://x", "o", "r", &Hash(hex_hash('e'))).unwrap();
        assert!(seeded.is_none());
        assert!(dst.get_commit(&Hash(hex_hash('e'))).unwrap().is_none());
    }

    #[test]
    fn seed_misses_when_repo_never_cached() {
        let root = TempDir::new().unwrap();
        state::set_mounts_root(root.path().to_path_buf());
        let dst_dir = TempDir::new().unwrap();
        let dst = SqliteRepository::open_relaxed(&dst_dir.path().join("cache.db")).unwrap();
        let seeded = try_seed(&dst, "https://x", "o", "r", &Hash(hex_hash('f'))).unwrap();
        assert!(seeded.is_none());
    }

    #[test]
    fn seed_refuses_incomplete_sizes() {
        let root = TempDir::new().unwrap();
        state::set_mounts_root(root.path().to_path_buf());

        let src_dir = TempDir::new().unwrap();
        let (src, commit, _) = seed_source_cache(src_dir.path());
        // Sizes file missing one of the manifest's blobs.
        let partial: HashMap<String, u64> = [(hex_hash('a'), 3u64)].into_iter().collect();
        populate(&src, "https://x", "o", "r", &commit.hash, &partial).unwrap();

        let dst_dir = TempDir::new().unwrap();
        let dst = SqliteRepository::open_relaxed(&dst_dir.path().join("cache.db")).unwrap();
        let seeded = try_seed(&dst, "https://x", "o", "r", &commit.hash).unwrap();
        assert!(
            seeded.is_none(),
            "a sizes file missing manifest blobs must not fast-path"
        );
    }

    #[test]
    fn populate_is_idempotent_across_mounts() {
        let root = TempDir::new().unwrap();
        state::set_mounts_root(root.path().to_path_buf());

        let src_dir = TempDir::new().unwrap();
        let (src, commit, sizes) = seed_source_cache(src_dir.path());
        populate(&src, "https://x", "o", "r", &commit.hash, &sizes).unwrap();
        // A second mount of the same head re-populates without error.
        populate(&src, "https://x", "o", "r", &commit.hash, &sizes).unwrap();

        let dst_dir = TempDir::new().unwrap();
        let dst = SqliteRepository::open_relaxed(&dst_dir.path().join("cache.db")).unwrap();
        assert!(try_seed(&dst, "https://x", "o", "r", &commit.hash)
            .unwrap()
            .is_some());
    }

    #[test]
    fn sanitize_component_flattens_urls() {
        assert_eq!(sanitize_component("https://oak.space"), "https---oak-space");
        assert_eq!(sanitize_component(""), "-");
        assert_eq!(sanitize_component("owner"), "owner");
    }
}
