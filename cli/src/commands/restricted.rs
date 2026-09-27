//! Client-side awareness of path-permission-withheld ("restricted") content.
//!
//! The oakspace server's path-based permissions (the `path-permissions`
//! feature) withhold the *content* of files under a restricted path while
//! still shipping the tree structure, so manifest hashes verify but the
//! withheld files can't materialize. Responses name the withheld blob hashes
//! (`restricted_blobs` on pull, `restricted` on `blobs/info` and
//! `chunks/download`); this module records them in repo metadata
//! ([`MetadataKey::RestrictedBlobs`]) so later commands can tell "restricted,
//! never materialized" apart from "deleted by the user" or "server data gap".
//!
//! Consumers must always pair the recorded set with a local-presence check
//! (`repo.has_blob`): once access is granted and the content pulled, a stale
//! recorded hash is inert.

use std::collections::HashSet;

use oak_core::{Manifest, MetadataKey, Repository, Result};

/// Standard remedy phrasing — the fix for restricted content is an access
/// grant from an org admin, not a re-pull or a server repair.
pub const ACCESS_HINT: &str = "ask an org admin for access";

/// Record server-reported restricted blob hashes (union with any already
/// recorded). No-op when `hashes` is empty or adds nothing new.
pub fn record_restricted_blobs(repo: &dyn Repository, hashes: &[String]) -> Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let mut set = load_restricted_blobs(repo);
    let before = set.len();
    set.extend(hashes.iter().cloned());
    if set.len() == before {
        return Ok(());
    }
    let mut list: Vec<&str> = set.iter().map(String::as_str).collect();
    list.sort_unstable();
    repo.set_metadata(MetadataKey::RestrictedBlobs, &list.join("\n"))
}

/// The recorded restricted blob-hash set. Tolerant: absent metadata (or a
/// backend without metadata support) reads as empty.
pub fn load_restricted_blobs(repo: &dyn Repository) -> HashSet<String> {
    repo.get_metadata(MetadataKey::RestrictedBlobs)
        .ok()
        .flatten()
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Paths in `manifest` whose blob the server withheld as restricted and whose
/// content is still absent locally — i.e. files that could not materialize
/// and must not be treated as deleted. Sorted.
pub fn restricted_paths_in_manifest(repo: &dyn Repository, manifest: &Manifest) -> Vec<String> {
    let set = load_restricted_blobs(repo);
    if set.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = manifest
        .entries
        .iter()
        .filter(|e| {
            set.contains(e.blob_hash.as_str()) && !repo.has_blob(&e.blob_hash).unwrap_or(false)
        })
        .map(|e| e.path.clone())
        .collect();
    out.sort();
    out
}

/// One-line summary for clone/pull output, e.g.
/// `2 file(s) are under restricted paths — content withheld by the server (ask an org admin for access)`.
pub fn withheld_summary(count: usize) -> String {
    format!("{count} file(s) are under restricted paths — content withheld by the server ({ACCESS_HINT})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{Blob, FileMode, Hash, ManifestEntry, SqliteRepository};
    use tempfile::TempDir;

    fn test_repo() -> (TempDir, SqliteRepository) {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        (temp, repo)
    }

    #[test]
    fn record_unions_and_loads_round_trip() {
        let (_t, repo) = test_repo();
        assert!(load_restricted_blobs(&repo).is_empty());

        record_restricted_blobs(&repo, &["b".repeat(64), "a".repeat(64)]).unwrap();
        record_restricted_blobs(&repo, &["a".repeat(64), "c".repeat(64)]).unwrap();
        // Recording nothing is a no-op, not a clear.
        record_restricted_blobs(&repo, &[]).unwrap();

        let set = load_restricted_blobs(&repo);
        assert_eq!(set.len(), 3);
        assert!(set.contains(&"a".repeat(64)));
        assert!(set.contains(&"c".repeat(64)));
    }

    #[test]
    fn restricted_paths_skip_locally_present_blobs() {
        let (_t, repo) = test_repo();
        let withheld = Hash("11".repeat(32));
        let granted_content = b"granted later".to_vec();
        let granted = oak_core::hash_bytes(&granted_content);
        repo.store_blob(&Blob {
            hash: granted.clone(),
            size: granted_content.len() as u64,
            content: granted_content,
        })
        .unwrap();
        record_restricted_blobs(
            &repo,
            &[withheld.as_str().to_string(), granted.as_str().to_string()],
        )
        .unwrap();

        let manifest = Manifest::new(vec![
            ManifestEntry {
                path: "secret/plan.md".to_string(),
                blob_hash: withheld.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "secret/granted.md".to_string(),
                blob_hash: granted,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "README.md".to_string(),
                blob_hash: Hash("22".repeat(32)),
                mode: FileMode::Regular,
            },
        ]);

        // Only the still-absent recorded blob's path is restricted: the
        // granted one has local content, and README was never recorded.
        assert_eq!(
            restricted_paths_in_manifest(&repo, &manifest),
            vec!["secret/plan.md".to_string()]
        );
    }
}
