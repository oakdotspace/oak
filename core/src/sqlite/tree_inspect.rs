//! Bounded whole-tree evidence for one pinned commit (fb-519, fb-522).
//!
//! Every tree is decoded and hash-verified, every listed file's logical bytes
//! are streamed through BLAKE3 (Oak identity) and SHA-256 (portable digest),
//! and budgets stop the listing at an explicit, deterministic truncation
//! point instead of silently omitting paths. Like file inspection, this
//! requires `open_read_only`: HEAD, commit, trees and blobs share one read
//! snapshot, and nothing is written, migrated or hydrated.
use super::file_inspect::{
    budget, check_inspectable_schema, database, failure, require_read_only, resolve_revision,
    validate_revision, verified_commit_root, verified_tree, verify_blob_into, Checked, Failure,
    FileInspectionProofScope, InspectionStatus, VerifiedBlob,
};
use super::SqliteRepository;
use crate::{hash_bytes, FileMode, Hash, OakError, Result, Tree, TreeEntryKind};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;

pub const DEFAULT_TREE_MAX_FILES: u64 = 100_000;
pub const MAX_TREE_MAX_FILES: u64 = 10_000_000;
pub const DEFAULT_TREE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_TREE_MAX_BYTES: u64 = 1 << 40;
/// Aggregate decoded canonical tree bytes for one inspection.
const WHOLE_TREE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub struct TreeInspectionLimits {
    pub max_files: u64,
    pub max_bytes: u64,
}

impl Default for TreeInspectionLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_TREE_MAX_FILES,
            max_bytes: DEFAULT_TREE_MAX_BYTES,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TreeInspection {
    pub requested_revision: String,
    pub commit: Option<String>,
    pub commit_verification: &'static str,
    pub root_tree: Option<String>,
    /// `verified` only when every listed tree and file verified. A budget
    /// truncation alone leaves this `verified`; check `complete`.
    pub status: InspectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// True only when every path under the root was listed and verified.
    pub complete: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TreeTruncation>,
    /// Files listed (verified or not).
    pub file_count: u64,
    /// Sum of listed files' verified logical sizes.
    pub logical_bytes: u64,
    pub verified_trees: usize,
    /// Listing order: depth-first by canonical entry name.
    pub order: &'static str,
    pub max_files: u64,
    pub max_bytes: u64,
    pub proof_scope: FileInspectionProofScope,
    pub files: Vec<TreeFileEvidence>,
}

#[derive(Debug, Serialize)]
pub struct TreeTruncation {
    /// `max_files` or `max_bytes`.
    pub limit: &'static str,
    /// First path not listed.
    pub next_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_declared_size: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct TreeFileEvidence {
    pub path: String,
    pub mode: FileMode,
    pub blob: String,
    /// Verified logical size (decoded bytes, never codec bytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub status: InspectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Receives each listed file's logical bytes as they are verified, so a
/// materializer never holds a whole file in memory (fb-522).
///
/// Protocol per file: `begin`, zero or more `write_chunk` calls with
/// decoded bytes that are NOT yet verified, then `finish` with the file's
/// evidence. The bytes are trustworthy only when `finish` sees
/// `status == verified`; otherwise the sink must discard them.
pub trait TreeFileSink {
    fn begin(&mut self, path: &str, mode: FileMode) -> Result<()>;
    fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn finish(&mut self, file: &TreeFileEvidence) -> Result<()>;
}

struct SinkWriter<'s>(&'s mut dyn TreeFileSink);
impl std::io::Write for SinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_chunk(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SqliteRepository {
    /// Verify and list one pinned commit's whole tree within `limits`.
    pub fn inspect_pinned_tree(
        &self,
        revision: &str,
        limits: TreeInspectionLimits,
    ) -> Result<TreeInspection> {
        self.walk_pinned_tree(revision, limits, None)
    }

    /// [`Self::inspect_pinned_tree`], streaming each file's bytes through
    /// `visitor` (for materialization) while they are hashed. A `begin` or
    /// `finish` error aborts the walk and is returned.
    pub fn visit_pinned_tree(
        &self,
        revision: &str,
        limits: TreeInspectionLimits,
        visitor: &mut dyn TreeFileSink,
    ) -> Result<TreeInspection> {
        self.walk_pinned_tree(revision, limits, Some(visitor))
    }

    fn walk_pinned_tree(
        &self,
        revision: &str,
        limits: TreeInspectionLimits,
        visitor: Option<&mut dyn TreeFileSink>,
    ) -> Result<TreeInspection> {
        if limits.max_files == 0 || limits.max_files > MAX_TREE_MAX_FILES {
            return Err(OakError::InvalidArgument(format!(
                "--max-files must be between 1 and {MAX_TREE_MAX_FILES}"
            )));
        }
        if limits.max_bytes > MAX_TREE_MAX_BYTES {
            return Err(OakError::InvalidArgument(format!(
                "--max-bytes must be at most {MAX_TREE_MAX_BYTES}"
            )));
        }
        validate_revision(revision)?;
        let conn = self.conn.lock().unwrap();
        require_read_only(&conn, "tree inspection")?;
        let mut out = TreeInspection {
            requested_revision: revision.into(),
            commit: None,
            commit_verification: "not_verified",
            root_tree: None,
            status: InspectionStatus::Verified,
            reason: None,
            complete: false,
            truncated: false,
            truncation: None,
            file_count: 0,
            logical_bytes: 0,
            verified_trees: 0,
            order: "depth_first_by_entry_name",
            max_files: limits.max_files,
            max_bytes: limits.max_bytes,
            proof_scope: FileInspectionProofScope {
                source: "local_sqlite_snapshot",
                exclusions: ["commit_files", "ancestor_closure", "remote_durability"],
            },
            files: Vec::new(),
        };
        let mut walk = Walk {
            conn: &conn,
            limits,
            tree_budget: WHOLE_TREE_BYTES,
            trees: HashMap::new(),
            blobs: HashMap::new(),
            visitor,
            stopped: false,
        };
        match walk.run(&mut out) {
            Ok(()) => {}
            Err(WalkError::Check(Failure(status, reason))) => {
                out.status = status;
                out.reason = Some(reason);
            }
            Err(WalkError::Visitor(error)) => return Err(error),
        }
        out.complete = out.status == InspectionStatus::Verified && !out.truncated;
        Ok(out)
    }
}

enum WalkError {
    Check(Failure),
    Visitor(OakError),
}
impl From<Failure> for WalkError {
    fn from(value: Failure) -> Self {
        Self::Check(value)
    }
}

struct Walk<'c, 'v> {
    conn: &'c Connection,
    limits: TreeInspectionLimits,
    tree_budget: u64,
    trees: HashMap<Hash, Tree>,
    blobs: HashMap<Hash, VerifiedBlob>,
    visitor: Option<&'v mut dyn TreeFileSink>,
    stopped: bool,
}

impl Walk<'_, '_> {
    fn run(&mut self, out: &mut TreeInspection) -> std::result::Result<(), WalkError> {
        check_inspectable_schema(self.conn)?;
        let revision = resolve_revision(self.conn, &out.requested_revision)?;
        let hash = Hash::from_hex(&revision).map_err(super::file_inspect::corrupt)?;
        out.commit = Some(hash.to_string());
        let root = verified_commit_root(self.conn, &hash)?;
        out.commit_verification = "verified_v1_header";
        out.root_tree = Some(root.to_string());
        self.dir(out, &root, "", 0)
    }

    fn tree(&mut self, hash: &Hash, out: &mut TreeInspection) -> Checked<Tree> {
        if let Some(tree) = self.trees.get(hash) {
            return Ok(tree.clone());
        }
        let tree = verified_tree(self.conn, hash, &mut self.tree_budget)?;
        out.verified_trees += 1;
        self.trees.insert(hash.clone(), tree.clone());
        Ok(tree)
    }

    fn dir(
        &mut self,
        out: &mut TreeInspection,
        hash: &Hash,
        prefix: &str,
        depth: usize,
    ) -> std::result::Result<(), WalkError> {
        if depth >= MAX_DEPTH {
            return Err(budget("tree depth exceeds the 128-level inspection budget").into());
        }
        let tree = self.tree(hash, out)?;
        for entry in &tree.entries {
            if self.stopped {
                return Ok(());
            }
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if path.len() > MAX_PATH_BYTES {
                return Err(budget("a tree path exceeds the 4096-byte inspection budget").into());
            }
            match entry.kind {
                TreeEntryKind::Tree => self.dir(out, &entry.hash, &path, depth + 1)?,
                TreeEntryKind::Blob => self.file(out, path, entry.mode, &entry.hash)?,
            }
        }
        Ok(())
    }

    fn file(
        &mut self,
        out: &mut TreeInspection,
        path: String,
        mode: FileMode,
        blob: &Hash,
    ) -> std::result::Result<(), WalkError> {
        let declared = declared_blob_size(self.conn, blob)?;
        if out.file_count >= self.limits.max_files {
            return self.truncate(out, "max_files", path, declared);
        }
        if let Some(size) = declared {
            if out.logical_bytes.saturating_add(size) > self.limits.max_bytes {
                return self.truncate(out, "max_bytes", path, declared);
            }
        }
        out.file_count += 1;
        let remaining = self.limits.max_bytes - out.logical_bytes;
        let mut evidence = TreeFileEvidence {
            path,
            mode,
            blob: blob.to_string(),
            size: None,
            sha256: None,
            status: InspectionStatus::Verified,
            reason: None,
        };
        let cached = if self.visitor.is_none() {
            self.blobs.get(blob).cloned()
        } else {
            None
        };
        let result = match cached {
            Some(verified) => Ok(verified),
            None => {
                let mut declared_size = None;
                let result = if let Some(visitor) = self.visitor.as_deref_mut() {
                    visitor
                        .begin(&evidence.path, mode)
                        .map_err(WalkError::Visitor)?;
                    verify_blob_into(
                        self.conn,
                        blob,
                        remaining,
                        &mut declared_size,
                        &mut SinkWriter(visitor),
                    )
                } else {
                    verify_blob_into(
                        self.conn,
                        blob,
                        remaining,
                        &mut declared_size,
                        &mut std::io::sink(),
                    )
                };
                if let Ok(verified) = &result {
                    if self.visitor.is_none() {
                        self.blobs.insert(blob.clone(), verified.clone());
                    }
                }
                result
            }
        };
        match result {
            Ok(verified) => {
                out.logical_bytes += verified.logical_size;
                evidence.size = Some(verified.logical_size);
                evidence.sha256 = Some(verified.sha256);
            }
            Err(Failure(status, reason)) => {
                evidence.status = status;
                evidence.reason = Some(reason.clone());
                if out.status == InspectionStatus::Verified {
                    out.status = status;
                    out.reason = Some(format!("{}: {reason}", evidence.path));
                }
            }
        }
        if let Some(visitor) = self.visitor.as_deref_mut() {
            visitor.finish(&evidence).map_err(WalkError::Visitor)?;
        }
        out.files.push(evidence);
        Ok(())
    }

    fn truncate(
        &mut self,
        out: &mut TreeInspection,
        limit: &'static str,
        next_path: String,
        next_declared_size: Option<u64>,
    ) -> std::result::Result<(), WalkError> {
        self.stopped = true;
        out.truncated = true;
        out.truncation = Some(TreeTruncation {
            limit,
            next_path,
            next_declared_size,
        });
        Ok(())
    }
}

/// Declared logical size, or `None` when the blob has no local row (the
/// canonical empty blob is implicitly zero bytes).
fn declared_blob_size(conn: &Connection, hash: &Hash) -> Checked<Option<u64>> {
    let size: Option<i64> = conn
        .query_row(
            "SELECT size FROM blobs WHERE hash=?1",
            [hash.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(database)?;
    match size {
        Some(size) => u64::try_from(size)
            .map(Some)
            .map_err(|_| failure(InspectionStatus::Corrupt, "negative blob size")),
        None if hash == &hash_bytes(&[]) => Ok(Some(0)),
        None => Ok(None),
    }
}
