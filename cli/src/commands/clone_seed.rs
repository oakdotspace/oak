//! `oak clone ORG/REPO DEST --from LOCAL_CHECKOUT` (fb-448, fb-335): seed a
//! clone's content chunks from an existing local checkout of the same
//! repository instead of downloading them.
//!
//! Trust model. Only immutable, content-addressed chunk bytes are reused, and
//! every reused chunk is re-hashed against the chunk hash the *server*
//! advertised for the clone's blobs before it is stored (the destination's
//! `store_chunk` hashes it again, and the ordinary blob fetch re-verifies
//! every present chunk before assembling blobs). Nothing else crosses from
//! the source: no credentials, locks, refs, branches, descriptions, metadata,
//! stat cache, working-tree files or dirty state. Commits, trees, blob
//! descriptors, refs and the selected head still come from the server through
//! the normal clone path, so the result is the same repository state a plain
//! clone produces; the seed only changes where verified bytes come from.
//!
//! Identity binding. The source must be an Oak checkout (not a mount) whose
//! recorded owner, repository name and normalized remote origin equal the
//! clone target's. That keeps a mistyped `--from` from silently mixing
//! repositories; the hash checks above are what make reuse safe regardless.
//!
//! The source database is opened read-only (no migration, pinned snapshot)
//! and no source lock is taken: reused bytes are verified by hash, so a
//! concurrent writer in the source can at worst reduce reuse. A source that
//! cannot be read after validation forfeits the seed (`skipped_reason`) and
//! the clone downloads normally; only a source whose identity now names a
//! different repository stops the clone.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oak_core::protocol::BlobData;
use oak_core::{Hash, MetadataKey, OakError, Repository, Result, SqliteRepository};
use serde::Serialize;

/// A validated `--from` source.
pub(crate) struct CloneSeedSource {
    root: PathBuf,
    db_path: PathBuf,
    owner: String,
    name: String,
    origin: String,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct CloneSeedReceipt {
    pub source: String,
    pub identity_verified: bool,
    pub source_origin: String,
    /// What may be copied from the source. Always "content_chunks_only".
    pub reuse_scope: &'static str,
    pub phases_seeded: Vec<&'static str>,
    /// The chunk counters below partition this one: every considered chunk
    /// is exactly one of already present, reused from source chunks, reused
    /// from source blobs, rejected, or absent.
    pub chunks_considered_unique: u64,
    pub chunks_already_present_unique: u64,
    pub chunks_reused_from_source_chunks: u64,
    pub chunks_reused_from_source_blobs: u64,
    /// The source held bytes for the chunk (a chunk row and/or its whole
    /// blob) but none verified or could be read; downloaded instead.
    pub chunks_rejected_hash_mismatch: u64,
    /// The source held neither the chunk nor its whole blob.
    pub chunks_absent_in_source: u64,
    /// Source reads that failed (diagnostic, not a chunk category).
    pub source_read_errors: u64,
    pub reused_logical_bytes: u64,
    /// Set when the seed was forfeited entirely and the clone downloaded
    /// normally (e.g. the source became unreadable after validation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
    /// Hashes stored from the seed (receipt accounting only).
    #[serde(skip)]
    pub seeded: HashSet<Hash>,
}

impl CloneSeedReceipt {
    pub fn reused_unique(&self) -> u64 {
        self.chunks_reused_from_source_chunks + self.chunks_reused_from_source_blobs
    }
}

fn identity_error(detail: String) -> OakError {
    OakError::InvalidArgument(format!(
        "clone --from refused: {detail}; nothing was created or downloaded"
    ))
}

fn read_identity(db_path: &Path) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let repo = SqliteRepository::open_read_only(db_path)?;
    Ok((
        repo.get_metadata(MetadataKey::RepoOwner)?,
        repo.get_metadata(MetadataKey::RepoName)?,
        repo.get_metadata(MetadataKey::RemoteUrl)?
            .as_deref()
            .and_then(super::push::normalize_remote_url),
    ))
}

/// Canonicalize the longest existing ancestor of `path` and re-append the
/// rest, so overlap checks see through symlinked parents of a destination
/// that does not exist yet.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cursor) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |acc: PathBuf, part| acc.join(part));
        }
        match (cursor.parent(), cursor.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cursor = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

impl CloneSeedSource {
    /// Validate `from` as a seed for cloning `owner/name` from `remote` into
    /// `dest`. Runs before any network request or destination mutation.
    pub fn open(from: &Path, remote: &str, owner: &str, name: &str, dest: &Path) -> Result<Self> {
        let root = std::fs::canonicalize(from).map_err(|error| {
            identity_error(format!(
                "source '{}' is not readable ({error})",
                from.display()
            ))
        })?;
        let oak_dir = root.join(".oak");
        let oak_meta = std::fs::symlink_metadata(&oak_dir).map_err(|_| {
            identity_error(format!(
                "source '{}' is not an Oak checkout root (no .oak directory)",
                root.display()
            ))
        })?;
        if !oak_meta.file_type().is_dir() {
            return Err(identity_error(
                "the source .oak is not a plain directory".to_string(),
            ));
        }
        let db_path = oak_dir.join("oak.db");
        if !std::fs::symlink_metadata(&db_path).is_ok_and(|m| m.file_type().is_file()) {
            return Err(identity_error(
                "the source has no regular .oak/oak.db".to_string(),
            ));
        }
        if oak_dir.join("CLONE_IN_PROGRESS").symlink_metadata().is_ok() {
            return Err(identity_error(
                "the source is itself an unfinished clone".to_string(),
            ));
        }
        let dest_abs = if dest.is_absolute() {
            dest.to_path_buf()
        } else {
            std::env::current_dir()?.join(dest)
        };
        let dest_canonical = canonicalize_existing_prefix(&dest_abs);
        if dest_canonical.starts_with(&root) || root.starts_with(&dest_canonical) {
            return Err(identity_error(
                "the destination and the source checkout overlap".to_string(),
            ));
        }
        let origin = super::push::normalize_remote_url(remote)
            .ok_or_else(|| identity_error("the clone remote is not a valid URL".to_string()))?;
        let (source_owner, source_name, source_origin) =
            read_identity(&db_path).map_err(|error| {
                identity_error(format!("the source database is unreadable ({error})"))
            })?;
        if source_owner.as_deref() != Some(owner) || source_name.as_deref() != Some(name) {
            return Err(identity_error(format!(
                "the source checkout is {}/{}, not {owner}/{name}",
                source_owner.as_deref().unwrap_or("<unlinked>"),
                source_name.as_deref().unwrap_or("<unlinked>")
            )));
        }
        if source_origin.as_deref() != Some(origin.as_str()) {
            return Err(identity_error(format!(
                "the source checkout's remote origin ({}) differs from the clone remote ({origin})",
                source_origin.as_deref().unwrap_or("<none>")
            )));
        }
        Ok(Self {
            root,
            db_path,
            owner: owner.to_string(),
            name: name.to_string(),
            origin,
        })
    }

    /// Store, into `dest`, every chunk the server's blob descriptors require
    /// that the source holds with verified content. Chunks already present
    /// in `dest` are left for the normal fetch to verify.
    ///
    /// Any failure to read the source (it became locked, replaced, migrated,
    /// moved or damaged after validation) forfeits reuse — recorded in
    /// `receipt.skipped_reason` / `source_read_errors` — and the ordinary
    /// fetch downloads those chunks. The only source condition that stops the
    /// clone is a readable source whose identity now names another
    /// repository. Destination write errors always propagate.
    pub fn seed_chunks(
        &self,
        dest: &SqliteRepository,
        blobs: &[BlobData],
        receipt: &mut CloneSeedReceipt,
    ) -> Result<()> {
        let source = match SqliteRepository::open_read_only(&self.db_path) {
            Ok(source) => source,
            Err(error) => {
                receipt.skipped_reason = Some(format!(
                    "source database could not be opened after validation ({error}); every chunk was downloaded instead"
                ));
                return Ok(());
            }
        };
        // Re-bind identity on the snapshot actually read.
        let identity = source
            .get_metadata(MetadataKey::RepoOwner)
            .and_then(|owner| {
                source
                    .get_metadata(MetadataKey::RepoName)
                    .map(|name| (owner, name))
            });
        match identity {
            Err(error) => {
                receipt.skipped_reason = Some(format!(
                    "source identity could not be re-read after validation ({error}); every chunk was downloaded instead"
                ));
                return Ok(());
            }
            Ok((owner, name))
                if owner.as_deref() != Some(self.owner.as_str())
                    || name.as_deref() != Some(self.name.as_str()) =>
            {
                return Err(OakError::InvalidArgument(format!(
                    "clone --from refused: the source checkout's identity changed during the clone (now {}/{}, expected {}/{}); the clone was stopped before its working tree was written and its partial destination is removed",
                    owner.as_deref().unwrap_or("<unlinked>"),
                    name.as_deref().unwrap_or("<unlinked>"),
                    self.owner,
                    self.name
                )));
            }
            Ok(_) => {}
        }
        receipt.phases_seeded.push("initial");
        let bulk = super::BulkTxn::begin(dest)?;
        let mut considered: HashSet<Hash> = HashSet::new();
        let mut reused_bytes_since_flush = 0u64;
        for blob in blobs {
            let Ok(blob_hash) = Hash::from_hex(&blob.hash) else {
                continue;
            };
            // Chunks of this blob still missing after the chunk-table pass,
            // with whether the source held unusable bytes for them.
            let mut missing: Vec<(Hash, u64, u32, bool)> = Vec::new();
            for chunk in &blob.chunks {
                let Ok(hash) = Hash::from_hex(&chunk.hash) else {
                    continue;
                };
                if !considered.insert(hash.clone()) {
                    continue;
                }
                receipt.chunks_considered_unique += 1;
                if dest.has_chunk(&hash)? {
                    receipt.chunks_already_present_unique += 1;
                    continue;
                }
                // A source read failure (malformed row, locked or damaged
                // database) only forfeits reuse; the server supplies the bytes.
                match source.get_chunk(&hash) {
                    Ok(Some(content)) if oak_core::hash_bytes(&content) == hash => {
                        dest.store_chunk(&hash, &content)?;
                        receipt.seeded.insert(hash);
                        receipt.chunks_reused_from_source_chunks += 1;
                        receipt.reused_logical_bytes += content.len() as u64;
                        reused_bytes_since_flush += content.len() as u64;
                    }
                    Ok(Some(_)) => missing.push((hash, chunk.offset, chunk.size, true)),
                    Ok(None) => missing.push((hash, chunk.offset, chunk.size, false)),
                    Err(_) => {
                        receipt.source_read_errors += 1;
                        missing.push((hash, chunk.offset, chunk.size, true));
                    }
                }
            }
            if !missing.is_empty() {
                // A checkout that committed or received the whole blob holds
                // its plaintext: slice the server-advertised ranges and keep
                // only slices whose hash matches. Compressed-frame chunk refs
                // simply fail the check and are downloaded instead.
                let (verified_blob, blob_unusable) = match source.get_blob(&blob_hash) {
                    Ok(Some(b)) if oak_core::hash_bytes(&b.content) == blob_hash => {
                        (Some(b), false)
                    }
                    Ok(Some(_)) => (None, true),
                    Ok(None) => (None, false),
                    Err(_) => {
                        receipt.source_read_errors += 1;
                        (None, true)
                    }
                };
                for (hash, offset, size, row_unusable) in missing {
                    let slice = verified_blob.as_ref().and_then(|b| {
                        let start = usize::try_from(offset).ok()?;
                        let end = start.checked_add(size as usize)?;
                        b.content.get(start..end)
                    });
                    // Exactly one category per chunk: recovered, rejected
                    // (the source held bytes, none verified), or absent.
                    match slice {
                        Some(bytes) if oak_core::hash_bytes(bytes) == hash => {
                            dest.store_chunk(&hash, bytes)?;
                            receipt.seeded.insert(hash);
                            receipt.chunks_reused_from_source_blobs += 1;
                            receipt.reused_logical_bytes += bytes.len() as u64;
                            reused_bytes_since_flush += bytes.len() as u64;
                        }
                        Some(_) => receipt.chunks_rejected_hash_mismatch += 1,
                        None if row_unusable || blob_unusable => {
                            receipt.chunks_rejected_hash_mismatch += 1
                        }
                        None => receipt.chunks_absent_in_source += 1,
                    }
                }
            }
            if reused_bytes_since_flush > 64 * 1024 * 1024 {
                bulk.flush()?;
                reused_bytes_since_flush = 0;
            }
        }
        bulk.commit()
    }

    pub fn receipt(&self) -> CloneSeedReceipt {
        CloneSeedReceipt {
            source: self.root.display().to_string(),
            identity_verified: true,
            source_origin: self.origin.clone(),
            reuse_scope: "content_chunks_only",
            ..CloneSeedReceipt::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::protocol::ChunkRefData;

    struct Fixture {
        _temp: tempfile::TempDir,
        source_db: PathBuf,
        seed: CloneSeedSource,
        dest: SqliteRepository,
    }

    const REMOTE: &str = "https://oak.example";

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(root.join("src/.oak")).unwrap();
        let source_db = root.join("src/.oak/oak.db");
        let source = SqliteRepository::open(&source_db).unwrap();
        source.set_metadata(MetadataKey::RemoteUrl, REMOTE).unwrap();
        source.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
        source.set_metadata(MetadataKey::RepoName, "repo").unwrap();
        drop(source);
        let seed =
            CloneSeedSource::open(&root.join("src"), REMOTE, "acme", "repo", &root.join("dst"))
                .unwrap();
        let dest = SqliteRepository::open(&root.join("dest.db")).unwrap();
        Fixture {
            _temp: temp,
            source_db,
            seed,
            dest,
        }
    }

    /// One two-chunk blob; returns its descriptor and chunk hashes.
    fn blob_descriptor(content: &[u8]) -> (BlobData, Vec<Hash>) {
        let mid = content.len() / 2;
        let parts = [&content[..mid], &content[mid..]];
        let hashes: Vec<Hash> = parts.iter().map(|p| oak_core::hash_bytes(p)).collect();
        let blob = BlobData {
            hash: oak_core::hash_bytes(content).to_string(),
            content: Vec::new(),
            size: content.len() as u64,
            chunks: vec![
                ChunkRefData {
                    hash: hashes[0].to_string(),
                    offset: 0,
                    size: mid as u32,
                },
                ChunkRefData {
                    hash: hashes[1].to_string(),
                    offset: mid as u64,
                    size: (content.len() - mid) as u32,
                },
            ],
            mapping_proof_token: None,
        };
        (blob, hashes)
    }

    fn assert_partition(r: &CloneSeedReceipt) {
        assert_eq!(
            r.chunks_considered_unique,
            r.chunks_already_present_unique
                + r.chunks_reused_from_source_chunks
                + r.chunks_reused_from_source_blobs
                + r.chunks_rejected_hash_mismatch
                + r.chunks_absent_in_source,
            "{r:?}"
        );
    }

    #[test]
    fn source_locked_after_validation_forfeits_the_seed_instead_of_failing() {
        let fx = fixture();
        let content = b"seed content that the source holds whole".to_vec();
        SqliteRepository::open(&fx.source_db)
            .unwrap()
            .put_blob(content.clone())
            .unwrap();
        let (blob, hashes) = blob_descriptor(&content);
        // Another process takes the source database exclusively between
        // validation and seeding.
        let locker = rusqlite::Connection::open(&fx.source_db).unwrap();
        locker
            .execute_batch(
                "PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; UPDATE metadata SET value = value;",
            )
            .unwrap();
        let mut receipt = fx.seed.receipt();
        fx.seed
            .seed_chunks(&fx.dest, &[blob], &mut receipt)
            .expect("an unreadable source must only forfeit reuse");
        assert!(receipt.skipped_reason.is_some(), "{receipt:?}");
        assert!(receipt.phases_seeded.is_empty());
        assert_eq!(receipt.reused_unique(), 0);
        assert!(!fx.dest.has_chunk(&hashes[0]).unwrap());
        drop(locker);
    }

    #[test]
    fn source_identity_swapped_after_validation_stops_with_an_accurate_message() {
        let fx = fixture();
        SqliteRepository::open(&fx.source_db)
            .unwrap()
            .set_metadata(MetadataKey::RepoName, "other")
            .unwrap();
        let (blob, _) = blob_descriptor(b"irrelevant content");
        let mut receipt = fx.seed.receipt();
        let error = fx
            .seed
            .seed_chunks(&fx.dest, &[blob], &mut receipt)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("identity changed during the clone"),
            "{error}"
        );
        assert!(!error.contains("nothing was created"), "{error}");
    }

    #[test]
    fn counters_partition_considered_chunks() {
        let fx = fixture();
        let whole = b"a whole blob held by the source, sliced on demand".to_vec();
        let other = b"a second blob whose stored copy is corrupt".to_vec();
        let absent = b"a third blob the source never had".to_vec();
        let (whole_blob, whole_hashes) = blob_descriptor(&whole);
        let (other_blob, _) = blob_descriptor(&other);
        let (absent_blob, _) = blob_descriptor(&absent);
        {
            let source = SqliteRepository::open(&fx.source_db).unwrap();
            source.put_blob(whole.clone()).unwrap();
            source.put_blob(other.clone()).unwrap();
            // A chunk row for `whole` with the right key but wrong bytes.
            source
                .store_chunk(&whole_hashes[0], &whole[..whole.len() / 2])
                .unwrap();
            drop(source);
            let conn = rusqlite::Connection::open(&fx.source_db).unwrap();
            conn.execute(
                "UPDATE chunks SET content = zeroblob(length(content)) WHERE hash = ?1",
                [whole_hashes[0].as_str()],
            )
            .unwrap();
            conn.execute(
                "UPDATE blobs SET content = zeroblob(length(content)) WHERE hash = ?1",
                [other_blob.hash.as_str()],
            )
            .unwrap();
        }
        let mut receipt = fx.seed.receipt();
        fx.seed
            .seed_chunks(
                &fx.dest,
                &[whole_blob, other_blob, absent_blob],
                &mut receipt,
            )
            .unwrap();
        assert_partition(&receipt);
        assert_eq!(receipt.chunks_considered_unique, 6);
        // The corrupt chunk row is recovered from the verified whole blob and
        // counted once, as reused.
        assert_eq!(receipt.chunks_reused_from_source_blobs, 2);
        // A present-but-corrupt whole blob is rejected, not absent.
        assert_eq!(receipt.chunks_rejected_hash_mismatch, 2);
        assert_eq!(receipt.chunks_absent_in_source, 2);
        assert_eq!(receipt.seeded.len(), 2);
    }
}
