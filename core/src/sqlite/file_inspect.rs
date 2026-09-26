//! Bounded local object evidence. No checkout reads, hydration, or database writes.
use super::SqliteRepository;
use crate::{hash_bytes, Commit, FileMode, Hash, OakError, Result, Tree, TreeEntryKind};
use rusqlite::{Connection, DatabaseName, OptionalExtension};
use serde::Serialize;
use std::{
    cell::Cell,
    io::{BufReader, Read},
    rc::Rc,
};

pub const DEFAULT_FILE_INSPECTION_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_FILE_INSPECTION_BYTES: u64 = 256 * 1024 * 1024;
const TREE_BYTES: u64 = 16 * 1024 * 1024;
const PATH_TREE_BYTES: u64 = 64 * 1024 * 1024;
const HEADER_BYTES: i64 = 64 * 1024;
const REFERENCE_BYTES: i64 = 64 * 1024;
const HEAD_PARENT_HOPS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionStatus {
    Verified,
    PathMissing,
    ObjectMissing,
    Corrupt,
    BudgetExceeded,
    Unsupported,
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct FileInspection {
    pub requested_revision: String,
    pub commit: Option<String>,
    pub commit_verification: &'static str,
    pub root_tree: Option<String>,
    pub path: String,
    pub tree: Option<String>,
    pub blob: Option<String>,
    pub mode: Option<FileMode>,
    pub declared_size: Option<u64>,
    pub verified_size: Option<u64>,
    pub verified_trees: usize,
    pub proof_scope: FileInspectionProofScope,
    pub status: InspectionStatus,
    pub reason: Option<String>,
    pub max_bytes: u64,
    /// SHA-256 of the verified logical bytes (never codec bytes). Present
    /// only when `status` is `verified`; the Oak identity remains `blob`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// How the local store holds this blob — codec and stored byte length.
    /// Separate from logical identity: `declared_size`/`verified_size` and
    /// `blob` always describe decoded logical bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageRepresentation>,
}

/// Local stored representation of one blob, as distinct from its logical
/// content. `stored_bytes` is the on-disk (possibly compressed) length.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StorageRepresentation {
    /// `sqlite_blob_row`, or `implicit_empty` for the canonical empty blob
    /// that has no stored row.
    pub representation: &'static str,
    /// `none` (raw), `zstd`, or `unknown`.
    pub codec: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_id: Option<i64>,
    pub stored_bytes: u64,
}

/// Result of one streamed, hash-verified blob read.
#[derive(Debug, Clone)]
pub(super) struct VerifiedBlob {
    pub logical_size: u64,
    pub sha256: String,
    pub storage: StorageRepresentation,
}

#[derive(Debug, Serialize)]
pub struct FileInspectionProofScope {
    pub source: &'static str,
    pub exclusions: [&'static str; 3],
}

#[derive(Debug)]
pub(super) struct Failure(pub InspectionStatus, pub String);
pub(super) type Checked<T> = std::result::Result<T, Failure>;
pub(super) fn failure(status: InspectionStatus, reason: impl Into<String>) -> Failure {
    Failure(status, reason.into())
}
pub(super) fn database(error: rusqlite::Error) -> Failure {
    use rusqlite::{ffi::ErrorCode, Error};

    let malformed = matches!(
        &error,
        Error::FromSqlConversionFailure(..)
            | Error::IntegralValueOutOfRange(..)
            | Error::Utf8Error(..)
            | Error::InvalidColumnType(..)
            | Error::BlobSizeError
    ) || matches!(
        &error,
        Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                ErrorCode::DatabaseCorrupt
                    | ErrorCode::NotADatabase
                    | ErrorCode::TypeMismatch
            )
    );
    failure(
        if malformed {
            InspectionStatus::Corrupt
        } else {
            InspectionStatus::Unavailable
        },
        error.to_string(),
    )
}
pub(super) fn corrupt(error: impl std::fmt::Display) -> Failure {
    failure(InspectionStatus::Corrupt, error.to_string())
}
pub(super) fn budget(reason: &str) -> Failure {
    failure(InspectionStatus::BudgetExceeded, reason)
}

impl SqliteRepository {
    /// Verify one path under one pinned commit. Requires `open_read_only` so
    /// HEAD, commit metadata and every object belong to the same read snapshot.
    /// V1 header verification binds the root tree, not the unhashed change list
    /// or ancestor closure. File content is hashed as logical bytes, not codec bytes.
    pub fn inspect_pinned_file(
        &self,
        revision: &str,
        path: &str,
        max_bytes: u64,
    ) -> Result<FileInspection> {
        self.inspect_pinned_file_into(revision, path, max_bytes, &mut std::io::sink())
    }

    /// [`Self::inspect_pinned_file`], streaming the decoded logical bytes
    /// into `sink` as they are hashed. The sink sees unverified bytes: a
    /// caller must discard them unless the returned status is `verified`
    /// (see `oak file inspect --output`, which stages into a temp file and
    /// publishes only after verification).
    pub fn inspect_pinned_file_into(
        &self,
        revision: &str,
        path: &str,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
    ) -> Result<FileInspection> {
        if max_bytes == 0 || max_bytes > MAX_FILE_INSPECTION_BYTES {
            return Err(OakError::InvalidArgument(
                "--max-bytes must be between 1 and 268435456".into(),
            ));
        }
        if path.len() > 4096
            || path.is_empty()
            || path.contains('\\')
            || (path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic())
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || path.chars().any(char::is_control)
        {
            return Err(OakError::InvalidPath("PATH must be a slash-separated repo-relative file path without empty, dot, parent or control components".into()));
        }
        if path.split('/').count() > 128 {
            return Err(OakError::InvalidArgument(
                "PATH exceeds the 128-component inspection budget".into(),
            ));
        }
        validate_revision(revision)?;
        let conn = self.conn.lock().unwrap();
        require_read_only(&conn, "file inspection")?;
        let mut evidence = FileInspection {
            requested_revision: revision.into(),
            commit: None,
            commit_verification: "not_verified",
            root_tree: None,
            path: path.into(),
            tree: None,
            blob: None,
            mode: None,
            declared_size: None,
            verified_size: None,
            verified_trees: 0,
            proof_scope: FileInspectionProofScope {
                source: "local_sqlite_snapshot",
                exclusions: ["commit_files", "ancestor_closure", "remote_durability"],
            },
            status: InspectionStatus::Verified,
            reason: None,
            max_bytes,
            content_sha256: None,
            storage: None,
        };
        if let Err(Failure(status, reason)) = inspect(&conn, &mut evidence, sink) {
            evidence.status = status;
            evidence.reason = Some(reason);
        }
        Ok(evidence)
    }
}

/// Refuse a writable connection: pinned inspections rely on the deferred
/// read transaction `open_read_only` starts, so every read shares a snapshot.
pub(super) fn require_read_only(conn: &Connection, what: &str) -> Result<()> {
    if !conn
        .is_readonly(DatabaseName::Main)
        .map_err(|e| OakError::Database(e.to_string()))?
    {
        return Err(OakError::InvalidArgument(format!(
            "{what} requires open_read_only"
        )));
    }
    Ok(())
}

/// Validate the `--at` form shared by pinned inspections.
pub(super) fn validate_revision(revision: &str) -> Result<()> {
    if revision != "HEAD" && (revision.len() != 64 || Hash::from_hex(revision).is_err()) {
        return Err(OakError::InvalidArgument(
            "--at requires HEAD or a full 64-character hexadecimal commit hash".into(),
        ));
    }
    Ok(())
}

/// Schema/hash-format preconditions shared by pinned object inspections.
pub(super) fn check_inspectable_schema(conn: &Connection) -> Checked<()> {
    for (table, column) in [("blobs", "codec"), ("trees", "content")] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
                [table, column],
                |row| row.get(0),
            )
            .map_err(database)?;
        if !exists {
            return Err(failure(
                InspectionStatus::Unsupported,
                format!("local schema lacks {table}.{column}; inspection never migrates databases"),
            ));
        }
    }
    let hash_format = bounded_metadata_text(conn, "hash_format", "hash format metadata")?;
    if !matches!(hash_format.as_deref(), None | Some("v1")) {
        return Err(failure(
            InspectionStatus::Unsupported,
            "repository hash format is unsupported by V1 file inspection",
        ));
    }
    Ok(())
}

fn inspect(
    conn: &Connection,
    out: &mut FileInspection,
    sink: &mut dyn std::io::Write,
) -> Checked<()> {
    check_inspectable_schema(conn)?;
    let revision = resolve_revision(conn, &out.requested_revision)?;
    let hash = Hash::from_hex(&revision).map_err(corrupt)?;
    out.commit = Some(hash.to_string());
    let root = verified_commit_root(conn, &hash)?;
    out.commit_verification = "verified_v1_header";
    out.root_tree = Some(root.to_string());
    let mut current = root;
    let mut remaining = PATH_TREE_BYTES;
    let components: Vec<_> = out.path.split('/').map(str::to_owned).collect();
    for (i, name) in components.iter().enumerate() {
        out.tree = Some(current.to_string());
        let tree = verified_tree(conn, &current, &mut remaining)?;
        out.verified_trees += 1;
        let entry = tree.get(name).ok_or_else(|| {
            failure(
                InspectionStatus::PathMissing,
                "path is absent from the verified tree",
            )
        })?;
        if i + 1 == components.len() {
            if entry.kind != TreeEntryKind::Blob {
                return Err(failure(
                    InspectionStatus::Unsupported,
                    "path names a directory, not a file",
                ));
            }
            out.blob = Some(entry.hash.to_string());
            out.mode = Some(entry.mode);
            let verified = verify_blob_into(
                conn,
                &entry.hash,
                out.max_bytes,
                &mut out.declared_size,
                sink,
            )?;
            out.verified_size = Some(verified.logical_size);
            out.content_sha256 = Some(verified.sha256);
            out.storage = Some(verified.storage);
            return Ok(());
        }
        if entry.kind != TreeEntryKind::Tree {
            return Err(failure(
                InspectionStatus::PathMissing,
                "a parent component is a file; symlinks are not followed",
            ));
        }
        current = entry.hash.clone();
    }
    unreachable!("validated nonempty path")
}

/// Resolve HEAD with the same attached/detached semantics as status, commit,
/// and `oak hash`, while keeping every ref read in this inspection snapshot.
pub(super) fn resolve_revision(conn: &Connection, requested: &str) -> Checked<String> {
    if requested != "HEAD" {
        return Ok(requested.to_string());
    }

    let current_branch = bounded_metadata_text(conn, "current_branch", "current branch metadata")?
        .filter(|name| !name.is_empty());

    let resolved = if let Some(mut branch) = current_branch {
        let mut seen = std::collections::HashSet::new();
        let mut resolved = None;
        for _ in 0..HEAD_PARENT_HOPS {
            if !seen.insert(branch.clone()) {
                return Err(corrupt("HEAD branch parent chain contains a cycle"));
            }
            if let Some(head) = bounded_branch_head(conn, &branch)? {
                resolved = Some(head);
                break;
            }
            let Some(parent) = bounded_parent_branch(conn, &branch)? else {
                return Err(failure(InspectionStatus::ObjectMissing, "HEAD is absent"));
            };
            branch = parent;
        }
        resolved.ok_or_else(|| budget("HEAD parent chain exceeds 128-hop verification budget"))?
    } else {
        bounded_metadata_text(conn, "head", "legacy HEAD metadata")?
            .ok_or_else(|| failure(InspectionStatus::ObjectMissing, "HEAD is absent"))?
    };

    Ok(resolved)
}

pub(super) fn bounded_metadata_text(
    conn: &Connection,
    key: &str,
    label: &str,
) -> Checked<Option<String>> {
    bounded_text(
        conn,
        "SELECT length(CAST(value AS BLOB)), \
         CASE WHEN length(CAST(value AS BLOB))<=?2 THEN value END \
         FROM metadata WHERE key=?1",
        rusqlite::params![key, REFERENCE_BYTES],
        label,
    )
}

pub(super) fn bounded_branch_head(conn: &Connection, branch: &str) -> Checked<Option<String>> {
    bounded_text(
        conn,
        "SELECT length(CAST(head_hash AS BLOB)), \
         CASE WHEN length(CAST(head_hash AS BLOB))<=?2 THEN head_hash END \
         FROM branch_heads WHERE branch_name=?1",
        rusqlite::params![branch, REFERENCE_BYTES],
        "branch head",
    )
}

pub(super) fn bounded_parent_branch(conn: &Connection, branch: &str) -> Checked<Option<String>> {
    bounded_text(
        conn,
        "SELECT coalesce(length(CAST(parent_branch AS BLOB)),0), \
         CASE WHEN parent_branch IS NULL OR length(CAST(parent_branch AS BLOB))<=?2 \
         THEN parent_branch END FROM branches WHERE name=?1",
        rusqlite::params![branch, REFERENCE_BYTES],
        "parent branch",
    )
}

fn bounded_text(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    label: &str,
) -> Checked<Option<String>> {
    let row = conn
        .query_row(sql, params, |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .optional()
        .map_err(database)?;
    match row {
        None => Ok(None),
        Some((length, _)) if length > REFERENCE_BYTES => Err(budget(&format!(
            "{label} exceeds 64 KiB reference-field verification budget"
        ))),
        Some((_, value)) => Ok(value),
    }
}

pub(super) fn verified_commit_root(conn: &Connection, hash: &Hash) -> Checked<Hash> {
    let length: Option<i64> = conn.query_row(
        "SELECT length(CAST(branch_name AS BLOB))+coalesce(length(CAST(parent_hash AS BLOB)),0)+coalesce(length(CAST(merge_parent_hash AS BLOB)),0)+length(CAST(manifest_hash AS BLOB))+length(CAST(author AS BLOB))+coalesce(length(CAST(message AS BLOB)),0)+length(CAST(timestamp AS BLOB)) FROM commits WHERE hash=?1", [hash.as_str()], |r| r.get(0)
    ).optional().map_err(database)?;
    let length = length.ok_or_else(|| {
        failure(
            InspectionStatus::ObjectMissing,
            format!("commit {hash} is unavailable locally"),
        )
    })?;
    if length > HEADER_BYTES {
        return Err(budget("commit header exceeds 64 KiB verification budget"));
    }
    let (branch, parent, merge_parent, root, author, message, timestamp) = conn.query_row(
        "SELECT branch_name,parent_hash,merge_parent_hash,manifest_hash,author,message,timestamp FROM commits WHERE hash=?1", [hash.as_str()],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, String>(6)?))
    ).map_err(database)?;
    let root = Hash::from_hex(&root).map_err(corrupt)?;
    let parent = parent
        .map(|h| Hash::from_hex(&h))
        .transpose()
        .map_err(corrupt)?;
    let merge_parent = merge_parent
        .map(|h| Hash::from_hex(&h))
        .transpose()
        .map_err(corrupt)?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp)
        .map_err(corrupt)?
        .with_timezone(&chrono::Utc);
    Commit::rehydrate_verified(
        hash,
        branch,
        parent,
        merge_parent,
        root.clone(),
        author,
        message,
        vec![],
        timestamp,
    )
    .map_err(corrupt)?;
    Ok(root)
}

struct StorageReader<R> {
    reader: R,
    failed: Rc<Cell<bool>>,
}
impl<R: Read> Read for StorageReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let result = self.reader.read(buf);
        if result.is_err() {
            self.failed.set(true);
        }
        result
    }
}
struct LogicalReader<'a> {
    reader: Box<dyn Read + 'a>,
    storage_failed: Rc<Cell<bool>>,
}
impl LogicalReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Checked<usize> {
        self.reader.read(buf).map_err(|error| {
            if self.storage_failed.get() {
                failure(
                    InspectionStatus::Unavailable,
                    format!("local storage read failed: {error}"),
                )
            } else {
                decode_error(error)
            }
        })
    }
}

fn decode_reader<'a>(reader: impl Read + 'a, codec: i64) -> Checked<LogicalReader<'a>> {
    let storage_failed = Rc::new(Cell::new(false));
    let reader = StorageReader {
        reader,
        failed: storage_failed.clone(),
    };
    let reader: Box<dyn Read + 'a> = match codec {
        0 => Box::new(reader),
        1 => {
            let mut decoder =
                zstd::stream::read::Decoder::new(BufReader::new(reader)).map_err(|error| {
                    if storage_failed.get() {
                        failure(InspectionStatus::Unavailable, error.to_string())
                    } else {
                        decode_error(error)
                    }
                })?;
            decoder.window_log_max(23).map_err(corrupt)?;
            Box::new(decoder)
        }
        _ => {
            return Err(failure(
                InspectionStatus::Unsupported,
                format!("unsupported blob codec {codec}"),
            ))
        }
    };
    Ok(LogicalReader {
        reader,
        storage_failed,
    })
}

fn decode_error(error: std::io::Error) -> Failure {
    if error.to_string().contains("too much memory") {
        budget("zstd frame exceeds the 8 MiB decoder window budget")
    } else {
        corrupt(error)
    }
}

pub(super) fn verified_tree(conn: &Connection, hash: &Hash, remaining: &mut u64) -> Checked<Tree> {
    if hash == &Tree::empty_hash() {
        return Ok(Tree::empty());
    }
    let row = conn
        .query_row(
            "SELECT rowid,length(content) FROM trees WHERE hash=?1",
            [hash.as_str()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<u64>>(1)?)),
        )
        .optional()
        .map_err(database)?;
    let (rowid, stored) = row.ok_or_else(|| {
        failure(
            InspectionStatus::ObjectMissing,
            format!("tree {hash} is unavailable locally"),
        )
    })?;
    let limit = TREE_BYTES.min(*remaining);
    let raw = if let Some(stored) = stored {
        if stored > TREE_BYTES {
            return Err(budget("stored tree exceeds 16 MiB budget"));
        }
        let blob = conn
            .blob_open(DatabaseName::Main, "trees", "content", rowid, true)
            .map_err(database)?;
        let mut raw = Vec::new();
        let mut reader = decode_reader(blob, 1)?;
        let mut buffer = [0u8; 64 * 1024];
        while raw.len() as u64 <= limit {
            let wanted = buffer.len().min((limit - raw.len() as u64 + 1) as usize);
            let count = reader.read(&mut buffer[..wanted])?;
            if count == 0 {
                break;
            }
            raw.extend_from_slice(&buffer[..count]);
        }
        raw
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT name,kind,hash,mode FROM tree_entries WHERE tree_hash=?1 ORDER BY name",
            )
            .map_err(database)?;
        let mut rows = stmt.query([hash.as_str()]).map_err(database)?;
        let mut raw = Vec::new();
        while let Some(row) = rows.next().map_err(database)? {
            // SQLite borrows row text here: check size before allocating it.
            let mut fields = Vec::with_capacity(4);
            for column in [1, 0, 2, 3] {
                let value = row
                    .get_ref(column)
                    .map_err(database)?
                    .as_str()
                    .map_err(corrupt)?;
                if value.len() as u64 > limit {
                    return Err(budget("legacy tree entry exceeds tree budget"));
                }
                fields.push(value);
            }
            let addition =
                fields.iter().map(|s| s.len()).sum::<usize>() + 3 + usize::from(!raw.is_empty());
            if raw.len() as u64 + addition as u64 > limit {
                return Err(budget("legacy tree exceeds path tree budget"));
            }
            if !raw.is_empty() {
                raw.push(b'\n');
            }
            // Legacy rows store regular mode for directories; canonical bytes use tree.
            if fields[0] == "tree" {
                fields[3] = "tree";
            }
            raw.extend_from_slice(fields.join("\t").as_bytes());
        }
        raw
    };
    if raw.len() as u64 > limit {
        return Err(budget(
            "decoded tree exceeds per-tree or aggregate path budget",
        ));
    }
    *remaining -= raw.len() as u64;
    if hash_bytes(&raw) != *hash {
        return Err(corrupt(format!("tree {hash} canonical hash mismatch")));
    }
    let tree = Tree::from_canonical_bytes(hash.clone(), &raw).map_err(corrupt)?;
    let rebuilt = Tree::new(tree.entries.clone()).map_err(corrupt)?;
    if rebuilt.canonical_bytes() != raw {
        return Err(corrupt(
            "tree entries are not canonical sorted unique entries",
        ));
    }
    Ok(tree)
}

/// Stream one blob's decoded logical bytes into `sink` while hashing them.
/// Verified only when the byte count equals the declared size and the BLAKE3
/// digest equals `hash`; the sink may already hold bytes on failure.
pub(super) fn verify_blob_into(
    conn: &Connection,
    hash: &Hash,
    max_bytes: u64,
    declared_size: &mut Option<u64>,
    sink: &mut dyn std::io::Write,
) -> Checked<VerifiedBlob> {
    use sha2::Digest;

    let row = conn
        .query_row(
            "SELECT rowid,size,codec,length(content) FROM blobs WHERE hash=?1",
            [hash.as_str()],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, u64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(database)?;
    let Some((rowid, size, codec, stored)) = row else {
        if hash == &hash_bytes(&[]) {
            *declared_size = Some(0);
            return Ok(VerifiedBlob {
                logical_size: 0,
                sha256: hex_lower(&sha2::Sha256::digest([])),
                storage: StorageRepresentation {
                    representation: "implicit_empty",
                    codec: "none",
                    codec_id: None,
                    stored_bytes: 0,
                },
            });
        }
        return Err(failure(
            InspectionStatus::ObjectMissing,
            format!("blob {hash} is unavailable locally"),
        ));
    };
    let size = u64::try_from(size).map_err(corrupt)?;
    *declared_size = Some(size);
    if size > max_bytes || stored > max_bytes + 1024 * 1024 {
        return Err(budget(
            "blob exceeds logical or stored-byte verification budget",
        ));
    }
    let storage = StorageRepresentation {
        representation: "sqlite_blob_row",
        codec: match codec {
            0 => "none",
            1 => "zstd",
            _ => "unknown",
        },
        codec_id: Some(codec),
        stored_bytes: stored,
    };
    let blob = conn
        .blob_open(DatabaseName::Main, "blobs", "content", rowid, true)
        .map_err(database)?;
    let mut reader = decode_reader(blob, codec)?;
    let mut digest = blake3::Hasher::new();
    let mut sha = sha2::Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let wanted = buffer.len().min((max_bytes - count + 1) as usize);
        let n = reader.read(&mut buffer[..wanted])?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > max_bytes {
            return Err(budget(
                "decoded blob exceeds logical-byte verification budget",
            ));
        }
        digest.update(&buffer[..n]);
        sha.update(&buffer[..n]);
        sink.write_all(&buffer[..n]).map_err(|error| {
            failure(
                InspectionStatus::Unavailable,
                format!("output write failed: {error}"),
            )
        })?;
    }
    if count != size {
        return Err(corrupt(format!(
            "blob size mismatch: metadata={size}, logical={count}"
        )));
    }
    if digest.finalize().to_hex().as_str() != hash.as_str() {
        return Err(corrupt(format!(
            "blob {hash} logical content hash mismatch"
        )));
    }
    Ok(VerifiedBlob {
        logical_size: count,
        sha256: hex_lower(&sha.finalize()),
        storage,
    })
}

pub(super) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingStorage;
    impl Read for FailingStorage {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("storage temporarily unavailable"))
        }
    }

    #[test]
    fn storage_failure_is_not_content_corruption_through_either_codec() {
        for codec in [0, 1] {
            let result = match decode_reader(FailingStorage, codec) {
                Ok(mut reader) => reader.read(&mut [0; 10]),
                Err(error) => Err(error),
            };
            assert_eq!(result.unwrap_err().0, InspectionStatus::Unavailable);
        }
        let mut malformed = decode_reader(b"not zstd".as_slice(), 1).unwrap();
        assert_eq!(
            malformed.read(&mut [0; 10]).unwrap_err().0,
            InspectionStatus::Corrupt
        );
    }
}
