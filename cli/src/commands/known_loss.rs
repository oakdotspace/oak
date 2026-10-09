//! Durable client-side record of operator-adjudicated unavailable content.

use std::collections::HashSet;

use oak_core::{Manifest, MetadataKey, Repository, Result};

pub const KNOWN_LOSS_PROTOCOL: &str = oak_core::protocol::KNOWN_LOSS_PROTOCOL;
pub const OPERATOR_LOSS_REASON: &str = "operator_adjudicated_loss";

pub fn record_known_lost_blobs(repo: &dyn Repository, hashes: &[String]) -> Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let mut known: HashSet<String> = repo
        .get_metadata(MetadataKey::KnownLostBlobs)?
        .map(|raw| {
            raw.lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let before = known.len();
    known.extend(hashes.iter().cloned());
    if known.len() == before {
        return Ok(());
    }
    let mut ordered: Vec<&str> = known.iter().map(String::as_str).collect();
    ordered.sort_unstable();
    repo.set_metadata(MetadataKey::KnownLostBlobs, &ordered.join("\n"))
}

/// Remove only hashes whose plaintext was reconstructed, content-hashed, and
/// committed to local storage. An omitted server marker is not evidence of
/// recovery and must never clear this durable record.
pub fn clear_recovered_known_lost_blobs(
    repo: &dyn Repository,
    hashes: impl IntoIterator<Item = String>,
) -> Result<()> {
    let mut known = load_known_lost_blobs(repo);
    let before = known.len();
    for hash in hashes {
        known.remove(hash.as_str());
    }
    if known.len() == before {
        return Ok(());
    }
    let mut ordered: Vec<&str> = known.iter().map(String::as_str).collect();
    ordered.sort_unstable();
    repo.set_metadata(MetadataKey::KnownLostBlobs, &ordered.join("\n"))
}

pub fn load_known_lost_blobs(repo: &dyn Repository) -> HashSet<String> {
    repo.get_metadata(MetadataKey::KnownLostBlobs)
        .ok()
        .flatten()
        .map(|raw| {
            raw.lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn known_lost_paths_in_manifest(repo: &dyn Repository, manifest: &Manifest) -> Vec<String> {
    let known = load_known_lost_blobs(repo);
    let mut paths: Vec<String> = manifest
        .entries
        .iter()
        .filter(|entry| {
            known.contains(entry.blob_hash.as_str())
                && !repo.has_blob(&entry.blob_hash).unwrap_or(false)
        })
        .map(|entry| entry.path.clone())
        .collect();
    paths.sort();
    paths
}

pub fn warning(count: usize) -> String {
    format!(
        "{count} historical blob version(s) are permanently unavailable on the server; Oak preserved repository history metadata and skipped only declared missing content"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{FileMode, Hash, Manifest, ManifestEntry, SqliteRepository};

    #[test]
    fn known_loss_round_trips_and_maps_to_missing_manifest_paths() {
        let temp = tempfile::tempdir().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let missing = Hash("ab".repeat(32));
        record_known_lost_blobs(&repo, &[missing.to_string()]).unwrap();
        assert!(load_known_lost_blobs(&repo).contains(missing.as_str()));

        let manifest = Manifest::new(vec![ManifestEntry {
            path: "historical.lock".to_string(),
            blob_hash: missing,
            mode: FileMode::Regular,
        }]);
        assert_eq!(
            known_lost_paths_in_manifest(&repo, &manifest),
            vec!["historical.lock"]
        );
    }
}
