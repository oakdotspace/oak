use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use crate::hash::hash_bytes;
use crate::{
    Blob, Branch, BranchStatus, ChangeType, ChunkInfo, Commit, FileChange, FileMode, Hash,
    Manifest, ManifestEntry, MetadataKey, OakError, Result, Tag, Tree, TreeEntry, TreeEntryKind,
};

use crate::traits::{Repository, StatCacheEntry};

const MIGRATION_BASELINE: &str = include_str!("migrations/sqlite/0000_baseline.sql");
const MIGRATION_0001_RELAX_BRANCH_FKS_AND_MESSAGE: &str =
    include_str!("migrations/sqlite/0001_relax_branch_fks_and_message.sql");
const MIGRATION_0002_RELAX_COMMITS_PARENT_HASH_FK: &str =
    include_str!("migrations/sqlite/0002_relax_commits_parent_hash_fk.sql");
const MIGRATION_0003_STAT_CACHE: &str = include_str!("migrations/sqlite/0003_stat_cache.sql");
const MIGRATION_0004_STAT_CACHE_CTIME: &str =
    include_str!("migrations/sqlite/0004_stat_cache_ctime.sql");
const MIGRATION_0005_MERGE_PARENT_INDEX: &str =
    include_str!("migrations/sqlite/0005_merge_parent_index.sql");
const MIGRATION_0006_DROP_REDUNDANT_TREE_ENTRIES_INDEX: &str =
    include_str!("migrations/sqlite/0006_drop_redundant_tree_entries_index.sql");
const MIGRATION_0007_BLOBS_CODEC: &str = include_str!("migrations/sqlite/0007_blobs_codec.sql");
const MIGRATION_0008_TREES_CONTENT: &str = include_str!("migrations/sqlite/0008_trees_content.sql");
const MIGRATION_0009_RELAX_BLOB_CHUNKS_FK: &str =
    include_str!("migrations/sqlite/0009_relax_blob_chunks_fk.sql");

/// SQLite-backed repository for local storage
pub struct SqliteRepository {
    conn: Mutex<Connection>,
}

/// One row per migration. Mirrors the structure on the postgres side. New
/// migrations append to `MIGRATIONS` below; existing entries are immutable
/// once shipped.
struct Migration {
    version: &'static str,
    sql: &'static str,
    required: bool,
}

/// Canonical ordering of migrations. Migrations 001..011 were squashed into
/// `0000_baseline` on 2026-05-05; the `migration_already_applied` heuristic
/// recognises previously-migrated repositories and marks the baseline as
/// applied without re-running its DDL.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "0000_baseline",
        sql: MIGRATION_BASELINE,
        required: true,
    },
    Migration {
        version: "0001_relax_branch_fks_and_message",
        sql: MIGRATION_0001_RELAX_BRANCH_FKS_AND_MESSAGE,
        required: true,
    },
    Migration {
        version: "0002_relax_commits_parent_hash_fk",
        sql: MIGRATION_0002_RELAX_COMMITS_PARENT_HASH_FK,
        required: true,
    },
    Migration {
        version: "0003_stat_cache",
        sql: MIGRATION_0003_STAT_CACHE,
        required: true,
    },
    Migration {
        version: "0004_stat_cache_ctime",
        sql: MIGRATION_0004_STAT_CACHE_CTIME,
        required: true,
    },
    Migration {
        version: "0005_merge_parent_index",
        sql: MIGRATION_0005_MERGE_PARENT_INDEX,
        required: true,
    },
    Migration {
        version: "0006_drop_redundant_tree_entries_index",
        sql: MIGRATION_0006_DROP_REDUNDANT_TREE_ENTRIES_INDEX,
        required: true,
    },
    Migration {
        version: "0007_blobs_codec",
        sql: MIGRATION_0007_BLOBS_CODEC,
        required: true,
    },
    Migration {
        version: "0008_trees_content",
        sql: MIGRATION_0008_TREES_CONTENT,
        required: true,
    },
    Migration {
        version: "0009_relax_blob_chunks_fk",
        sql: MIGRATION_0009_RELAX_BLOB_CHUNKS_FK,
        required: true,
    },
];

/// zstd level for at-rest compression. 3 is the default — fast, and tree
/// content / source blobs are highly compressible, so a higher level buys
/// little for the per-write cost during bulk import.
const ZSTD_LEVEL: i32 = 3;

/// `blobs.codec` markers.
const CODEC_RAW: i64 = 0;
const CODEC_ZSTD: i64 = 1;

fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(data, ZSTD_LEVEL)
        .map_err(|e| OakError::Database(format!("zstd compress: {e}")))
}

fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(data).map_err(|e| OakError::Database(format!("zstd decompress: {e}")))
}

/// Encode blob content for storage: zstd when it actually shrinks, else raw.
/// Returns the bytes to store and the codec marker. Hashing is always over the
/// plaintext, so this choice never affects a blob's content-addressed identity.
fn encode_blob(content: &[u8]) -> Result<(std::borrow::Cow<'_, [u8]>, i64)> {
    let compressed = zstd_compress(content)?;
    if compressed.len() < content.len() {
        Ok((std::borrow::Cow::Owned(compressed), CODEC_ZSTD))
    } else {
        Ok((std::borrow::Cow::Borrowed(content), CODEC_RAW))
    }
}

/// Decode stored blob content given its codec marker.
fn decode_blob(content: Vec<u8>, codec: i64) -> Result<Vec<u8>> {
    if codec == CODEC_ZSTD {
        zstd_decompress(&content)
    } else {
        Ok(content)
    }
}

/// Insert a tree in the new on-disk format (migration 0008): its canonical hash
/// preimage, zstd-compressed, as a single `trees.content` row — no per-entry
/// `tree_entries` rows. `conn` may be a plain connection, a transaction, or a
/// savepoint (all deref to `Connection`).
fn insert_tree_row(conn: &Connection, tree: &Tree) -> Result<()> {
    let content = zstd_compress(&tree.canonical_bytes())?;
    conn.execute(
        "INSERT OR IGNORE INTO trees (hash, content) VALUES (?1, ?2)",
        params![tree.hash.as_str(), content],
    )
    .map_err(|e| OakError::Database(e.to_string()))?;
    Ok(())
}

/// Read a legacy tree (one stored as `tree_entries` rows, i.e. `trees.content`
/// is NULL). Used as the fallback path in `get_tree` for repos written before
/// migration 0008 / not yet compacted.
fn read_tree_entries(conn: &Connection, hash: &Hash) -> Result<Tree> {
    let mut stmt = conn
        .prepare("SELECT name, kind, hash, mode FROM tree_entries WHERE tree_hash = ?1")
        .map_err(|e| OakError::Database(e.to_string()))?;

    let mut entries: Vec<TreeEntry> = stmt
        .query_map(params![hash.as_str()], |row| {
            let name: String = row.get(0)?;
            let kind_str: String = row.get(1)?;
            let entry_hash: String = row.get(2)?;
            let mode_str: String = row.get(3)?;

            let kind = match kind_str.as_str() {
                "tree" => TreeEntryKind::Tree,
                _ => TreeEntryKind::Blob,
            };
            let mode = match mode_str.as_str() {
                "executable" => FileMode::Executable,
                "symlink" => FileMode::Symlink,
                _ => FileMode::Regular,
            };
            Ok(TreeEntry {
                name,
                kind,
                hash: Hash(entry_hash),
                mode,
            })
        })
        .map_err(|e| OakError::Database(e.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| OakError::Database(e.to_string()))?;

    // Re-sort to match Tree::new canonical ordering (PK guarantees uniqueness,
    // not row order).
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Tree {
        hash: hash.clone(),
        entries,
    })
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![name],
        |row| row.get(0),
    )
}

/// Heuristic: has this migration already been applied? A positive result
/// lets the runner record the version without re-executing its DDL — used
/// when an existing repository (created under the pre-squash chain) is
/// opened by a build that only knows about the baseline.
fn migration_already_applied(version: &str, conn: &Connection) -> Result<bool> {
    match version {
        // Baseline is "applied" iff the schema already has the canonical
        // `commits` table — created by the original 001_initial and never
        // dropped, so its presence implies the squashed baseline's effects
        // are already in place.
        "0000_baseline" => {
            table_exists(conn, "commits").map_err(|e| OakError::Database(e.to_string()))
        }
        // 0001 rebuilds `commits`, `branches`, `branch_heads` to drop the
        // strict FKs on branch_name/parent_branch and to make
        // `commits.message` nullable. A freshly-created DB (built from the
        // current `0000_baseline`) already has the relaxed schema, so the
        // migration is a no-op there. Detect that case by checking whether
        // the stored CREATE TABLE for `commits` still has `message TEXT
        // NOT NULL` — the marker of the pre-relax schema.
        "0001_relax_branch_fks_and_message" => {
            let sql: Option<String> = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='commits'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| OakError::Database(e.to_string()))?;
            // No `commits` table at all means the baseline hasn't run yet;
            // leave to the baseline migration to set up. If `commits` is
            // present and `message` is already nullable, the relax-pass is
            // implicit and we mark this version applied without re-running.
            Ok(sql
                .map(|s| !s.contains("message TEXT NOT NULL"))
                .unwrap_or(false))
        }
        // 0002 rebuilds `commits` to drop the FK on `parent_hash`. The
        // current baseline still has that FK, so a freshly-applied
        // baseline always needs this migration to run; only DBs that have
        // already been migrated (the stored `commits` CREATE TABLE no
        // longer mentions a FOREIGN KEY clause for parent_hash) should
        // mark this version applied without re-running.
        "0002_relax_commits_parent_hash_fk" => {
            let sql: Option<String> = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='commits'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| OakError::Database(e.to_string()))?;
            Ok(sql
                .map(|s| !s.contains("FOREIGN KEY (parent_hash)"))
                .unwrap_or(false))
        }
        _ => Ok(false),
    }
}

fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now')),
            succeeded INTEGER NOT NULL DEFAULT 1
        )",
    )
    .map_err(|e| OakError::Database(e.to_string()))?;

    for m in MIGRATIONS {
        let already: Option<i64> = conn
            .query_row(
                "SELECT succeeded FROM schema_migrations WHERE version = ?1",
                params![m.version],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;
        if already == Some(1) {
            continue;
        }

        // Tracker doesn't know about this migration. If the schema already
        // reflects it (squashed baseline running against a database that
        // was migrated under the old per-step chain), mark applied without
        // re-running the DDL.
        if migration_already_applied(m.version, conn)? {
            conn.execute(
                "INSERT INTO schema_migrations (version, succeeded) VALUES (?1, 1)
                 ON CONFLICT(version) DO UPDATE SET succeeded = 1",
                params![m.version],
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
            continue;
        }

        match conn.execute_batch(m.sql) {
            Ok(_) => {
                conn.execute(
                    "INSERT INTO schema_migrations (version, succeeded) VALUES (?1, 1)
                     ON CONFLICT(version) DO UPDATE SET succeeded = 1",
                    params![m.version],
                )
                .map_err(|e| OakError::Database(e.to_string()))?;
            }
            Err(e) if m.required => {
                return Err(OakError::Database(format!(
                    "migration {} failed: {}",
                    m.version, e
                )));
            }
            Err(_) => {
                // Best-effort migration — record as attempted so we don't
                // retry every connect, but don't fail.
                conn.execute(
                    "INSERT INTO schema_migrations (version, succeeded) VALUES (?1, 0)
                     ON CONFLICT(version) DO NOTHING",
                    params![m.version],
                )
                .map_err(|e| OakError::Database(e.to_string()))?;
            }
        }
    }
    Ok(())
}

impl SqliteRepository {
    /// Open or create a SQLite repository at the given path
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path).map_err(|e| OakError::Database(e.to_string()))?;

        // Enable WAL mode for concurrent multi-process access
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| OakError::Database(e.to_string()))?;

        run_migrations(&conn)?;

        Ok(SqliteRepository {
            conn: Mutex::new(conn),
        })
    }

    /// Open or create a SQLite repository at the given path with foreign-key
    /// enforcement disabled.
    ///
    /// Used by `oak mount` for its lazy blob cache: the mount stores a
    /// manifest whose entries reference blobs that haven't been fetched
    /// yet. The default schema declares `manifest_entries.blob_hash` as a
    /// foreign key to `blobs(hash)`; rusqlite enables enforcement by
    /// default, so inserts would fail. The mount cache is a private,
    /// per-mount store — relaxing FK enforcement is safe there because
    /// nothing else points at it.
    pub fn open_relaxed(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path).map_err(|e| OakError::Database(e.to_string()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=OFF;")
            .map_err(|e| OakError::Database(e.to_string()))?;
        run_migrations(&conn)?;
        Ok(SqliteRepository {
            conn: Mutex::new(conn),
        })
    }

    /// Toggle foreign-key enforcement on the open connection.
    ///
    /// The server's PostgreSQL schema is more permissive than the client's
    /// SQLite schema: `parent_branch` and `parent_hash` are unenforced on the
    /// server but FK-checked here. Bulk imports (e.g. `oak clone`) need to relax
    /// enforcement so that legitimate server data — branches whose parent has
    /// been deleted, or commits whose timestamp ordering doesn't match
    /// topology — can be ingested. Always re-enable after the bulk load so
    /// subsequent normal operations stay validated.
    pub fn set_foreign_keys(&self, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let pragma = if enabled {
            "PRAGMA foreign_keys=ON;"
        } else {
            "PRAGMA foreign_keys=OFF;"
        };
        conn.execute_batch(pragma)
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    /// Begin a relaxed-durability transaction for a bulk ingest (clone / pull).
    ///
    /// Lowers `synchronous` to `NORMAL` and opens a transaction, so the many
    /// per-object INSERTs that follow commit as a batch instead of fsync-ing
    /// once per statement. The default `synchronous=FULL` in WAL mode fsyncs
    /// on every auto-commit, and the clone ingest writes each chunk, blob,
    /// `blob_chunks` row, tree, and commit as its own statement — so that
    /// per-statement fsync dominates clone time on repos with many files.
    /// Subsequent `store_*` calls join this open transaction (SQLite leaves
    /// auto-commit mode while a manual `BEGIN` is active).
    ///
    /// Pair with [`Self::bulk_flush`] (called periodically to bound WAL
    /// growth on a large import) and exactly one of [`Self::bulk_commit`] /
    /// [`Self::bulk_rollback`]. Single-writer only: the caller must not issue
    /// writes through another handle to the same database while the batch is
    /// open, or that write will silently join — and ride the fate of — this
    /// transaction.
    pub fn bulk_begin(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("PRAGMA synchronous=NORMAL; BEGIN;")
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    /// Commit the current bulk batch and immediately open the next, so a long
    /// import doesn't accumulate its entire dataset in the WAL before a single
    /// final commit. Cheap under `synchronous=NORMAL` — no fsync until the
    /// next checkpoint.
    pub fn bulk_flush(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("COMMIT; BEGIN;")
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    /// Commit the bulk transaction and restore full durability.
    pub fn bulk_commit(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("COMMIT; PRAGMA synchronous=FULL;")
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    /// Roll back an in-flight bulk transaction and restore full durability.
    /// Best-effort, for the error path so a failed import doesn't leave a
    /// dangling transaction or a half-applied batch behind.
    pub fn bulk_rollback(&self) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute_batch("ROLLBACK; PRAGMA synchronous=FULL;");
        }
    }

    /// Create an in-memory repository (for testing)
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| OakError::Database(e.to_string()))?;
        run_migrations(&conn)?;
        Ok(SqliteRepository {
            conn: Mutex::new(conn),
        })
    }

    /// Open a [`BulkImporter`] against the same database file.
    ///
    /// The importer holds its own connection (so it can manage long-lived
    /// transactions independently of the regular repo's mutex), batches writes
    /// across `batch_size` commits, and caches inserted-tree hashes in memory
    /// to skip per-subtree EXISTS lookups during deeply repetitive imports
    /// (e.g. `oak clone <git-url>` or `oak init` over an existing `.git`).
    ///
    /// The caller must not concurrently issue writes via the underlying
    /// `SqliteRepository` until the importer is dropped or finished —
    /// SQLite only allows one writer at a time, and a clash will block.
    pub fn bulk_importer(&self, batch_size: usize) -> Result<BulkImporter> {
        let conn = self.conn.lock().unwrap();
        let path = conn
            .path()
            .ok_or_else(|| {
                OakError::Database("bulk importer requires a file-backed connection".to_string())
            })?
            .to_string();
        drop(conn);
        BulkImporter::open(Path::new(&path), batch_size)
    }

    /// One-shot storage compaction. Converts the database to the compact
    /// on-disk format and reclaims the freed space:
    ///   1. Backfills `trees.content` for every legacy tree (one still stored as
    ///      `tree_entries` rows), verifying each reconstructed tree re-hashes to
    ///      its key before writing.
    ///   2. Drops the now-unused `tree_entries` table (only if every tree has
    ///      `content`, so the fallback path is never needed again).
    ///   3. Re-compresses legacy raw blobs (`codec = 0`) that actually shrink.
    ///   4. `VACUUM`s to shrink the file on disk.
    ///
    /// Idempotent: a second run finds nothing to backfill/recompress and just
    /// VACUUMs. `VACUUM` needs transient free disk roughly equal to the final
    /// database size.
    pub fn compact_storage(&self) -> Result<CompactStats> {
        let mut conn = self.conn.lock().unwrap();

        // 1. Backfill legacy trees (content IS NULL) from tree_entries.
        let legacy_trees: Vec<String> = {
            let table = table_exists(&conn, "tree_entries")
                .map_err(|e| OakError::Database(e.to_string()))?;
            if !table {
                Vec::new() // already compacted away
            } else {
                let mut stmt = conn
                    .prepare("SELECT hash FROM trees WHERE content IS NULL")
                    .map_err(|e| OakError::Database(e.to_string()))?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|e| OakError::Database(e.to_string()))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| OakError::Database(e.to_string()))?
            }
        };

        let mut trees_backfilled = 0u64;
        if !legacy_trees.is_empty() {
            let tx = conn
                .transaction()
                .map_err(|e| OakError::Database(e.to_string()))?;
            for h in &legacy_trees {
                let hash = Hash(h.clone());
                let tree = read_tree_entries(&tx, &hash)?;
                // Integrity: the reconstructed tree must re-hash to its key.
                if hash_bytes(&tree.canonical_bytes()) != hash {
                    return Err(OakError::Database(format!(
                        "tree {h} failed integrity check during compaction"
                    )));
                }
                let content = zstd_compress(&tree.canonical_bytes())?;
                tx.execute(
                    "UPDATE trees SET content = ?2 WHERE hash = ?1",
                    params![h, content],
                )
                .map_err(|e| OakError::Database(e.to_string()))?;
                trees_backfilled += 1;
            }
            tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        }

        // 2. Drop tree_entries once every tree has content.
        let remaining_null: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM trees WHERE content IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        let tree_entries_dropped = if remaining_null == 0
            && table_exists(&conn, "tree_entries").map_err(|e| OakError::Database(e.to_string()))?
        {
            conn.execute_batch("DROP TABLE IF EXISTS tree_entries;")
                .map_err(|e| OakError::Database(e.to_string()))?;
            true
        } else {
            false
        };

        // 3. Re-compress legacy raw blobs that shrink.
        let raw_blobs: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT hash FROM blobs WHERE codec = 0")
                .map_err(|e| OakError::Database(e.to_string()))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| OakError::Database(e.to_string()))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| OakError::Database(e.to_string()))?
        };

        let mut blobs_recompressed = 0u64;
        if !raw_blobs.is_empty() {
            let tx = conn
                .transaction()
                .map_err(|e| OakError::Database(e.to_string()))?;
            for h in &raw_blobs {
                let content: Vec<u8> = tx
                    .query_row(
                        "SELECT content FROM blobs WHERE hash = ?1",
                        params![h],
                        |r| r.get(0),
                    )
                    .map_err(|e| OakError::Database(e.to_string()))?;
                let compressed = zstd_compress(&content)?;
                if compressed.len() < content.len() {
                    tx.execute(
                        "UPDATE blobs SET content = ?2, codec = 1 WHERE hash = ?1",
                        params![h, compressed],
                    )
                    .map_err(|e| OakError::Database(e.to_string()))?;
                    blobs_recompressed += 1;
                }
            }
            tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        }

        // 4. VACUUM (must run outside any transaction) to reclaim freed pages.
        conn.execute_batch("VACUUM;")
            .map_err(|e| OakError::Database(e.to_string()))?;

        Ok(CompactStats {
            trees_backfilled,
            tree_entries_dropped,
            blobs_recompressed,
        })
    }

    /// Replace a blob's chunk mapping wholesale. `store_blob_chunks` is
    /// `INSERT OR IGNORE` (append-only), which is right for ingest but can't
    /// correct an existing mapping — needed when a blob arrives mis-chunked
    /// from a skewed server and the client re-chunks the repaired plaintext.
    pub fn replace_blob_chunks(&self, blob_hash: &Hash, chunks: &[ChunkInfo]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        // Savepoint, not `transaction()`: pull ingest calls this inside an
        // already-open `BulkTxn` (same rationale as `store_tree`).
        let tx = conn
            .savepoint()
            .map_err(|e| OakError::Database(e.to_string()))?;
        tx.execute(
            "DELETE FROM blob_chunks WHERE blob_hash = ?1",
            params![blob_hash.as_str()],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        for (index, chunk) in chunks.iter().enumerate() {
            tx.execute(
                "INSERT INTO blob_chunks (blob_hash, chunk_hash, chunk_index, offset, size) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    blob_hash.as_str(),
                    chunk.hash.as_str(),
                    index as i64,
                    chunk.offset as i64,
                    chunk.length as i64,
                ],
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        }
        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }
}

/// Outcome of [`SqliteRepository::compact_storage`].
#[derive(Debug, Clone, Default)]
pub struct CompactStats {
    /// Legacy trees converted from `tree_entries` rows to `trees.content`.
    pub trees_backfilled: u64,
    /// Whether the `tree_entries` table was dropped (all trees now compact).
    pub tree_entries_dropped: bool,
    /// Legacy raw blobs re-compressed with zstd.
    pub blobs_recompressed: u64,
}

/// Bulk-write helper for git import (and similar one-shot replays).
///
/// Wraps a private connection to the repo's SQLite database that:
///   - Runs with relaxed durability pragmas (`synchronous=NORMAL`, larger cache,
///     `temp_store=MEMORY`) — appropriate for a re-runnable bulk import.
///   - Holds an outer transaction across `batch_size` commits, flushed
///     automatically when the threshold is reached. This collapses thousands
///     of per-commit `tx.commit()` fsyncs into a handful.
///   - Caches inserted tree hashes in a `HashSet` so unchanged subtrees skip
///     the SQL `SELECT EXISTS` lookup on every commit.
///
/// `finish()` flushes the final batch and must be called for the import to
/// be durable. If the importer is dropped without finishing, the open
/// transaction is rolled back.
pub struct BulkImporter {
    conn: Connection,
    tree_cache: HashSet<Hash>,
    uncommitted: usize,
    batch_size: usize,
    in_tx: bool,
}

impl BulkImporter {
    fn open(db_path: &Path, batch_size: usize) -> Result<Self> {
        let conn = Connection::open(db_path).map_err(|e| OakError::Database(e.to_string()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-262144;
             BEGIN;",
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(BulkImporter {
            conn,
            tree_cache: HashSet::new(),
            uncommitted: 0,
            batch_size: batch_size.max(1),
            in_tx: true,
        })
    }

    /// Hash and insert a blob's content into the open batch.
    pub fn put_blob(&mut self, content: Vec<u8>) -> Result<Hash> {
        let blob = Blob::new(content);
        let (stored, codec) = encode_blob(&blob.content)?;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO blobs (hash, content, size, codec) VALUES (?1, ?2, ?3, ?4)",
                params![blob.hash.as_str(), stored.as_ref(), blob.size as i64, codec],
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(blob.hash)
    }

    /// Build a tree from manifest entries and insert all subtrees into the
    /// open batch. Subtrees already inserted in this importer session are
    /// skipped without a roundtrip; subtrees already present on disk from
    /// prior work fall through to `INSERT OR IGNORE` and silently no-op.
    pub fn put_tree(&mut self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        let built = crate::build_tree(&entries)?;
        for tree in &built.trees {
            if !self.tree_cache.insert(tree.hash.clone()) {
                continue;
            }
            insert_tree_row(&self.conn, tree)?;
        }
        Ok(built.root_hash)
    }

    /// Insert a commit (plus its `commit_files`, if any) and tick the batch
    /// counter. Flushes the current transaction when the threshold is hit.
    pub fn store_commit(&mut self, commit: &Commit) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO commits (hash, branch_name, parent_hash, merge_parent_hash, manifest_hash, author, message, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    commit.hash.as_str(),
                    commit.branch_name,
                    commit.parent_hash.as_ref().map(|h| h.as_str()),
                    commit.merge_parent_hash.as_ref().map(|h| h.as_str()),
                    commit.manifest_hash.as_str(),
                    commit.author,
                    commit.message,
                    commit.timestamp.to_rfc3339(),
                ],
            )
            .map_err(|e| OakError::Database(e.to_string()))?;

        for file in &commit.files {
            let change_type_str = match file.change_type {
                ChangeType::Added => "added",
                ChangeType::Modified => "modified",
                ChangeType::Deleted => "deleted",
                ChangeType::Renamed => "renamed",
            };
            self.conn
                .execute(
                    "INSERT INTO commit_files (commit_hash, path, change_type, old_blob_hash, new_blob_hash, old_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        commit.hash.as_str(),
                        file.path,
                        change_type_str,
                        file.old_blob_hash.as_ref().map(|h| h.as_str()),
                        file.new_blob_hash.as_ref().map(|h| h.as_str()),
                        file.old_path,
                    ],
                )
                .map_err(|e| OakError::Database(e.to_string()))?;
        }

        self.uncommitted += 1;
        if self.uncommitted >= self.batch_size {
            self.flush_batch()?;
        }
        Ok(())
    }

    fn flush_batch(&mut self) -> Result<()> {
        if !self.in_tx {
            return Ok(());
        }
        self.conn
            .execute_batch("COMMIT; BEGIN;")
            .map_err(|e| OakError::Database(e.to_string()))?;
        self.uncommitted = 0;
        Ok(())
    }

    /// Commit the final batch. After this the importer is durable on disk.
    pub fn finish(mut self) -> Result<()> {
        if self.in_tx {
            self.conn
                .execute_batch("COMMIT;")
                .map_err(|e| OakError::Database(e.to_string()))?;
            self.in_tx = false;
        }
        Ok(())
    }
}

impl Drop for BulkImporter {
    fn drop(&mut self) {
        if self.in_tx {
            // Best-effort rollback so a panicked / errored import doesn't leave
            // a half-applied batch behind. `finish()` clears `in_tx` first, so
            // a clean finish path skips this.
            let _ = self.conn.execute_batch("ROLLBACK;");
        }
    }
}

impl Repository for SqliteRepository {
    fn store_blob(&self, blob: &Blob) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let (stored, codec) = encode_blob(&blob.content)?;
        conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, content, size, codec) VALUES (?1, ?2, ?3, ?4)",
            params![blob.hash.as_str(), stored.as_ref(), blob.size as i64, codec],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn get_blob(&self, hash: &Hash) -> Result<Option<Blob>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<(Vec<u8>, i64, i64)> = conn
            .query_row(
                "SELECT content, size, codec FROM blobs WHERE hash = ?1",
                params![hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;

        match result {
            None => Ok(None),
            Some((content, size, codec)) => Ok(Some(Blob {
                hash: hash.clone(),
                content: decode_blob(content, codec)?,
                size: size as u64,
            })),
        }
    }

    fn has_blob(&self, hash: &Hash) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)",
                params![hash.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(exists)
    }

    /// Write a manifest as a tree. The hash of `manifest` is already the tree
    /// root hash (computed by `Manifest::new` via `build_tree`), so this is
    /// just a tree-store of the corresponding nested structure.
    fn store_manifest(&self, manifest: &Manifest) -> Result<()> {
        self.put_tree(manifest.entries.clone()).map(|_| ())
    }

    /// Read a manifest by hash. Reads from the tree tables.
    fn get_manifest(&self, hash: &Hash) -> Result<Option<Manifest>> {
        if self.has_tree(hash)? {
            let entries = self.walk_tree(hash)?;
            return Ok(Some(Manifest {
                hash: hash.clone(),
                entries,
            }));
        }
        Ok(None)
    }

    // --- Tree operations ---

    fn store_tree(&self, tree: &Tree) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        // Use a savepoint, not `transaction()`: clone/pull ingest calls this
        // inside an already-open bulk transaction (`BulkTxn`), and SQLite
        // rejects a nested `BEGIN` ("cannot start a transaction within a
        // transaction"). A savepoint nests cleanly inside the bulk txn and, when
        // there's no outer transaction (a standalone call), still commits on its
        // own — so these writes stay atomic in both cases.
        let tx = conn
            .savepoint()
            .map_err(|e| OakError::Database(e.to_string()))?;

        insert_tree_row(&tx, tree)?;

        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn get_tree(&self, hash: &Hash) -> Result<Option<Tree>> {
        if hash == &Tree::empty_hash() {
            return Ok(Some(Tree::empty()));
        }
        let conn = self.conn.lock().unwrap();

        // New format (migration 0008): a single compressed-content row.
        // `content` is NULL for legacy trees still stored as tree_entries rows.
        let row: Option<Option<Vec<u8>>> = conn
            .query_row(
                "SELECT content FROM trees WHERE hash = ?1",
                params![hash.as_str()],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;

        match row {
            None => Ok(None), // no such tree
            Some(Some(content)) => {
                let raw = zstd_decompress(&content)?;
                Ok(Some(Tree::from_canonical_bytes(hash.clone(), &raw)?))
            }
            Some(None) => Ok(Some(read_tree_entries(&conn, hash)?)), // legacy fallback
        }
    }

    fn has_tree(&self, hash: &Hash) -> Result<bool> {
        if hash == &Tree::empty_hash() {
            return Ok(true);
        }
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM trees WHERE hash = ?1)",
                params![hash.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(exists)
    }

    /// Optimized `put_tree`: builds the nested tree, then inserts all tree
    /// objects in a single transaction, skipping subtrees whose hash already
    /// exists. This is the key write-path optimization for tree storage.
    fn put_tree(&self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        let built = crate::build_tree(&entries)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| OakError::Database(e.to_string()))?;

        for tree in &built.trees {
            let already_present: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM trees WHERE hash = ?1)",
                    params![tree.hash.as_str()],
                    |row| row.get(0),
                )
                .map_err(|e| OakError::Database(e.to_string()))?;
            if already_present {
                continue;
            }

            insert_tree_row(&tx, tree)?;
        }

        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(built.root_hash)
    }

    // --- Branch operations ---

    fn store_branch(&self, branch: &Branch) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO branches (name, description, parent_branch, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                branch.name,
                branch.description,
                branch.parent_branch,
                branch.status.as_str(),
                branch.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn get_branch(&self, name: &str) -> Result<Option<Branch>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<(Option<String>, Option<String>, String, String)> = conn
            .query_row(
                "SELECT description, parent_branch, status, created_at FROM branches WHERE name = ?1",
                params![name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;

        match result {
            Some((description, parent_branch, status_str, created_at_str)) => {
                let status = BranchStatus::from_db_str(&status_str);
                let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                    .map_err(|e| OakError::Database(e.to_string()))?
                    .with_timezone(&chrono::Utc);
                Ok(Some(Branch {
                    name: name.to_string(),
                    description,
                    parent_branch,
                    status,
                    created_at,
                }))
            }
            None => Ok(None),
        }
    }

    fn list_branches(&self) -> Result<Vec<Branch>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name, description, parent_branch, status, created_at FROM branches ORDER BY created_at ASC")
            .map_err(|e| OakError::Database(e.to_string()))?;

        let branches = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let description: Option<String> = row.get(1)?;
                let parent_branch: Option<String> = row.get(2)?;
                let status_str: String = row.get(3)?;
                let created_at_str: String = row.get(4)?;
                Ok((name, description, parent_branch, status_str, created_at_str))
            })
            .map_err(|e| OakError::Database(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| OakError::Database(e.to_string()))?;

        branches
            .into_iter()
            .map(
                |(name, description, parent_branch, status_str, created_at_str)| {
                    let status = BranchStatus::from_db_str(&status_str);
                    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                        .map_err(|e| OakError::Database(e.to_string()))?
                        .with_timezone(&chrono::Utc);
                    Ok(Branch {
                        name,
                        description,
                        parent_branch,
                        status,
                        created_at,
                    })
                },
            )
            .collect()
    }

    fn update_branch_status(&self, name: &str, status: BranchStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE branches SET status = ?1 WHERE name = ?2",
            params![status.as_str(), name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn update_branch_description(&self, name: &str, description: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE branches SET description = ?1 WHERE name = ?2",
            params![description, name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn rename_branch(&self, old_name: &str, new_name: &str) -> Result<()> {
        if old_name == new_name {
            return Ok(());
        }
        if new_name.is_empty() {
            return Err(OakError::Database("new branch name cannot be empty".into()));
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| OakError::Database(e.to_string()))?;

        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM branches WHERE name = ?1",
                params![old_name],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?
            .unwrap_or(false);
        if !exists {
            return Err(OakError::BranchNotFound(old_name.to_string()));
        }

        let conflict: bool = tx
            .query_row(
                "SELECT 1 FROM branches WHERE name = ?1",
                params![new_name],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?
            .unwrap_or(false);
        if conflict {
            return Err(OakError::BranchAlreadyExists(new_name.to_string()));
        }

        // Insert renamed row, repoint all references, then delete the old row.
        // Insert-before-update keeps FK semantics correct if foreign_keys are
        // ever turned on; the order below is safe with FKs OFF too.
        tx.execute(
            "INSERT INTO branches (name, description, parent_branch, status, created_at) \
             SELECT ?1, description, parent_branch, status, created_at FROM branches WHERE name = ?2",
            params![new_name, old_name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        tx.execute(
            "UPDATE branches SET parent_branch = ?1 WHERE parent_branch = ?2",
            params![new_name, old_name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        tx.execute(
            "UPDATE commits SET branch_name = ?1 WHERE branch_name = ?2",
            params![new_name, old_name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        tx.execute(
            "UPDATE branch_heads SET branch_name = ?1 WHERE branch_name = ?2",
            params![new_name, old_name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        tx.execute(
            "UPDATE metadata SET value = ?1 WHERE key = ?2 AND value = ?3",
            params![new_name, MetadataKey::CurrentBranch.as_str(), old_name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        tx.execute("DELETE FROM branches WHERE name = ?1", params![old_name])
            .map_err(|e| OakError::Database(e.to_string()))?;

        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn delete_branch(&self, name: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| OakError::Database(e.to_string()))?;
        tx.execute(
            "DELETE FROM branch_heads WHERE branch_name = ?1",
            params![name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        tx.execute("DELETE FROM branches WHERE name = ?1", params![name])
            .map_err(|e| OakError::Database(e.to_string()))?;
        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn get_branch_head(&self, name: &str) -> Result<Option<Hash>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<String> = conn
            .query_row(
                "SELECT head_hash FROM branch_heads WHERE branch_name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(result.map(Hash))
    }

    fn set_branch_head(&self, name: &str, hash: &Hash) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO branch_heads (branch_name, head_hash) VALUES (?1, ?2)",
            params![name, hash.as_str()],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn reconcile_remote_merge_branch_state(
        &self,
        closed_branch: &str,
        new_branch: &Branch,
        main_head: &Hash,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| OakError::Database(e.to_string()))?;

        tx.execute(
            "UPDATE branches SET status = ?1 WHERE name = ?2",
            params![BranchStatus::Closed.as_str(), closed_branch],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        tx.execute(
            "INSERT OR IGNORE INTO branches (name, description, parent_branch, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                new_branch.name,
                new_branch.description,
                new_branch.parent_branch,
                new_branch.status.as_str(),
                new_branch.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        tx.execute(
            "INSERT OR REPLACE INTO branch_heads (branch_name, head_hash) VALUES (?1, ?2)",
            params![new_branch.name, main_head.as_str()],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        tx.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            params![MetadataKey::Head.as_str(), main_head.as_str()],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        tx.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            params![MetadataKey::CurrentBranch.as_str(), new_branch.name],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    // --- Commit operations ---

    fn store_commit(&self, commit: &Commit) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        // Savepoint rather than `transaction()` for the same reason as
        // `store_tree`: clone/pull's `store_pull_response` calls this inside an
        // open `BulkTxn`, where a nested `BEGIN` would error. A savepoint nests
        // there and still commits standalone.
        let tx = conn
            .savepoint()
            .map_err(|e| OakError::Database(e.to_string()))?;

        tx.execute(
            "INSERT OR IGNORE INTO commits (hash, branch_name, parent_hash, merge_parent_hash, manifest_hash, author, message, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                commit.hash.as_str(),
                commit.branch_name,
                commit.parent_hash.as_ref().map(|h| h.as_str()),
                commit.merge_parent_hash.as_ref().map(|h| h.as_str()),
                commit.manifest_hash.as_str(),
                commit.author,
                commit.message,
                commit.timestamp.to_rfc3339(),
            ],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        for file in &commit.files {
            let change_type_str = match file.change_type {
                ChangeType::Added => "added",
                ChangeType::Modified => "modified",
                ChangeType::Deleted => "deleted",
                ChangeType::Renamed => "renamed",
            };

            tx.execute(
                "INSERT INTO commit_files (commit_hash, path, change_type, old_blob_hash, new_blob_hash, old_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    commit.hash.as_str(),
                    file.path,
                    change_type_str,
                    file.old_blob_hash.as_ref().map(|h| h.as_str()),
                    file.new_blob_hash.as_ref().map(|h| h.as_str()),
                    file.old_path,
                ],
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        }

        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn get_commit(&self, hash: &Hash) -> Result<Option<Commit>> {
        let conn = self.conn.lock().unwrap();

        #[allow(clippy::type_complexity)]
        let result: Option<(String, Option<String>, Option<String>, String, String, Option<String>, String)> = conn
            .query_row(
                "SELECT branch_name, parent_hash, merge_parent_hash, manifest_hash, author, message, timestamp FROM commits WHERE hash = ?1",
                params![hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;

        let (
            branch_name,
            parent_hash,
            merge_parent_hash,
            manifest_hash,
            author,
            message,
            timestamp_str,
        ) = match result {
            Some(r) => r,
            None => return Ok(None),
        };

        let mut stmt = conn
            .prepare("SELECT path, change_type, old_blob_hash, new_blob_hash, old_path FROM commit_files WHERE commit_hash = ?1")
            .map_err(|e| OakError::Database(e.to_string()))?;

        let files: Vec<FileChange> = stmt
            .query_map(params![hash.as_str()], |row| {
                let path: String = row.get(0)?;
                let change_type_str: String = row.get(1)?;
                let old_blob_hash: Option<String> = row.get(2)?;
                let new_blob_hash: Option<String> = row.get(3)?;
                let old_path: Option<String> = row.get(4)?;

                let change_type = match change_type_str.as_str() {
                    "added" => ChangeType::Added,
                    "deleted" => ChangeType::Deleted,
                    "renamed" => ChangeType::Renamed,
                    _ => ChangeType::Modified,
                };

                Ok(FileChange {
                    path,
                    change_type,
                    old_blob_hash: old_blob_hash.map(Hash),
                    new_blob_hash: new_blob_hash.map(Hash),
                    old_path,
                    old_mode: None,
                    new_mode: None,
                })
            })
            .map_err(|e| OakError::Database(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| OakError::Database(e.to_string()))?;

        let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
            .map_err(|e| OakError::Database(e.to_string()))?
            .with_timezone(&chrono::Utc);

        Ok(Some(Commit {
            hash: hash.clone(),
            branch_name,
            parent_hash: parent_hash.map(Hash),
            merge_parent_hash: merge_parent_hash.map(Hash),
            manifest_hash: Hash(manifest_hash),
            author,
            message,
            timestamp,
            files,
        }))
    }

    fn has_commit(&self, hash: &Hash) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM commits WHERE hash = ?1",
                params![hash.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(count > 0)
    }

    fn get_commits_for_branch(&self, branch_name: &str) -> Result<Vec<Commit>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT hash FROM commits WHERE branch_name = ?1 ORDER BY timestamp ASC")
            .map_err(|e| OakError::Database(e.to_string()))?;

        let hashes: Vec<String> = stmt
            .query_map(params![branch_name], |row| row.get(0))
            .map_err(|e| OakError::Database(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| OakError::Database(e.to_string()))?;

        drop(stmt);
        drop(conn);

        let mut commits = Vec::new();
        for hash in hashes {
            if let Some(edit) = self.get_commit(&Hash(hash))? {
                commits.push(edit);
            }
        }
        Ok(commits)
    }

    fn count_commits_for_branch(&self, branch_name: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM commits WHERE branch_name = ?1",
                params![branch_name],
                |row| row.get(0),
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(count as usize)
    }

    fn merge_child_exists(&self, branch_head: &Hash) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM commits WHERE merge_parent_hash = ?1)",
                params![branch_head.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(exists)
    }

    fn get_commits_since(&self, branch_name: &str, since: Option<&Hash>) -> Result<Vec<Commit>> {
        let all = self.get_commits_for_branch(branch_name)?;

        match since {
            None => Ok(all),
            Some(since_hash) => {
                let mut found = false;
                let mut result = Vec::new();
                for commit in &all {
                    if &commit.hash == since_hash {
                        found = true;
                    } else if found {
                        result.push(commit.clone());
                    }
                }
                // If since_hash wasn't found in this branch's commits (e.g.
                // it belongs to a different branch), return all commits —
                // they are all new from the caller's perspective.
                if !found {
                    return Ok(all);
                }
                Ok(result)
            }
        }
    }

    fn get_all_commits(&self) -> Result<Vec<Commit>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT hash FROM commits ORDER BY timestamp ASC")
            .map_err(|e| OakError::Database(e.to_string()))?;

        let hashes: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| OakError::Database(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| OakError::Database(e.to_string()))?;

        drop(stmt);
        drop(conn);

        let mut commits = Vec::new();
        for hash in hashes {
            if let Some(edit) = self.get_commit(&Hash(hash))? {
                commits.push(edit);
            }
        }
        Ok(commits)
    }

    // --- Metadata ---

    fn get_metadata(&self, key: MetadataKey) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                params![key.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(result)
    }

    fn set_metadata(&self, key: MetadataKey, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            params![key.as_str(), value],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    // --- Working-tree stat cache ---

    fn load_stat_cache(&self) -> Result<HashMap<String, StatCacheEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT path, mtime_ns, ctime_ns, size, blob_hash FROM stat_cache")
            .map_err(|e| OakError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    StatCacheEntry {
                        mtime_ns: row.get(1)?,
                        ctime_ns: row.get(2)?,
                        size: row.get(3)?,
                        blob_hash: Hash(row.get::<_, String>(4)?),
                    },
                ))
            })
            .map_err(|e| OakError::Database(e.to_string()))?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, entry) = row.map_err(|e| OakError::Database(e.to_string()))?;
            map.insert(path, entry);
        }
        Ok(map)
    }

    fn update_stat_cache(
        &self,
        upserts: &[(String, StatCacheEntry)],
        removed: &[String],
    ) -> Result<()> {
        if upserts.is_empty() && removed.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| OakError::Database(e.to_string()))?;
        {
            let mut upsert = tx
                .prepare(
                    "INSERT INTO stat_cache (path, mtime_ns, ctime_ns, size, blob_hash) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(path) DO UPDATE SET
                         mtime_ns = excluded.mtime_ns,
                         ctime_ns = excluded.ctime_ns,
                         size = excluded.size,
                         blob_hash = excluded.blob_hash",
                )
                .map_err(|e| OakError::Database(e.to_string()))?;
            for (path, entry) in upserts {
                upsert
                    .execute(params![
                        path,
                        entry.mtime_ns,
                        entry.ctime_ns,
                        entry.size,
                        entry.blob_hash.as_str()
                    ])
                    .map_err(|e| OakError::Database(e.to_string()))?;
            }
            let mut delete = tx
                .prepare("DELETE FROM stat_cache WHERE path = ?1")
                .map_err(|e| OakError::Database(e.to_string()))?;
            for path in removed {
                delete
                    .execute(params![path])
                    .map_err(|e| OakError::Database(e.to_string()))?;
            }
        }
        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    // --- Tag operations ---

    fn create_tag(&self, name: &str, commit_hash: &Hash) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tags (name, commit_hash, created_at) VALUES (?1, ?2, ?3)",
            params![name, commit_hash.as_str(), chrono::Utc::now().to_rfc3339()],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn list_tags(&self) -> Result<Vec<Tag>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name, commit_hash, created_at FROM tags ORDER BY created_at ASC")
            .map_err(|e| OakError::Database(e.to_string()))?;

        let tags = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let commit_hash: String = row.get(1)?;
                let created_at_str: String = row.get(2)?;
                Ok((name, commit_hash, created_at_str))
            })
            .map_err(|e| OakError::Database(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| OakError::Database(e.to_string()))?;

        tags.into_iter()
            .map(|(name, commit_hash, created_at_str)| {
                let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                    .map_err(|e| OakError::Database(e.to_string()))?
                    .with_timezone(&chrono::Utc);
                Ok(Tag {
                    name,
                    commit_hash: Hash(commit_hash),
                    created_at,
                })
            })
            .collect()
    }

    fn delete_tag(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tags WHERE name = ?1", params![name])
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    // --- Chunk operations ---

    fn store_chunk(&self, hash: &Hash, content: &[u8]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO chunks (hash, content, size) VALUES (?1, ?2, ?3)",
            params![hash.as_str(), content, content.len() as i64],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }

    fn get_chunk(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<Vec<u8>> = conn
            .query_row(
                "SELECT content FROM chunks WHERE hash = ?1",
                params![hash.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(result)
    }

    fn has_chunk(&self, hash: &Hash) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM chunks WHERE hash = ?1)",
                params![hash.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        Ok(exists)
    }

    fn store_blob_chunks(&self, blob_hash: &Hash, chunks: &[ChunkInfo]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for (index, chunk) in chunks.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO blob_chunks (blob_hash, chunk_hash, chunk_index, offset, size) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    blob_hash.as_str(),
                    chunk.hash.as_str(),
                    index as i64,
                    chunk.offset as i64,
                    chunk.length as i64,
                ],
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        }
        Ok(())
    }

    fn get_blob_chunks(&self, blob_hash: &Hash) -> Result<Option<Vec<ChunkInfo>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT chunk_hash, offset, size FROM blob_chunks WHERE blob_hash = ?1 ORDER BY chunk_index",
            )
            .map_err(|e| OakError::Database(e.to_string()))?;

        let rows: Vec<ChunkInfo> = stmt
            .query_map(params![blob_hash.as_str()], |row| {
                let hash_str: String = row.get(0)?;
                let offset: i64 = row.get(1)?;
                let size: i64 = row.get(2)?;
                Ok(ChunkInfo {
                    hash: Hash(hash_str),
                    offset: offset as u64,
                    length: size as u32,
                })
            })
            .map_err(|e| OakError::Database(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| OakError::Database(e.to_string()))?;

        if rows.is_empty() {
            Ok(None)
        } else {
            Ok(Some(rows))
        }
    }

    fn atomic_commit(&self, manifest: &Manifest, commit: &Commit, branch_name: &str) -> Result<()> {
        // Build tree structure outside the transaction so the lock is held briefly.
        let built = crate::build_tree(&manifest.entries)?;

        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| OakError::Database(e.to_string()))?;

        // Store every tree object (skipping ones already present).
        for tree in &built.trees {
            let already_present: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM trees WHERE hash = ?1)",
                    params![tree.hash.as_str()],
                    |row| row.get(0),
                )
                .map_err(|e| OakError::Database(e.to_string()))?;
            if already_present {
                continue;
            }
            insert_tree_row(&tx, tree)?;
        }

        // Store commit + files
        tx.execute(
            "INSERT OR IGNORE INTO commits (hash, branch_name, parent_hash, merge_parent_hash, manifest_hash, author, message, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                commit.hash.as_str(),
                commit.branch_name,
                commit.parent_hash.as_ref().map(|h| h.as_str()),
                commit.merge_parent_hash.as_ref().map(|h| h.as_str()),
                commit.manifest_hash.as_str(),
                commit.author,
                commit.message,
                commit.timestamp.to_rfc3339(),
            ],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        for file in &commit.files {
            let change_type_str = match file.change_type {
                ChangeType::Added => "added",
                ChangeType::Modified => "modified",
                ChangeType::Deleted => "deleted",
                ChangeType::Renamed => "renamed",
            };
            tx.execute(
                "INSERT INTO commit_files (commit_hash, path, change_type, old_blob_hash, new_blob_hash, old_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    commit.hash.as_str(),
                    file.path,
                    change_type_str,
                    file.old_blob_hash.as_ref().map(|h| h.as_str()),
                    file.new_blob_hash.as_ref().map(|h| h.as_str()),
                    file.old_path,
                ],
            )
            .map_err(|e| OakError::Database(e.to_string()))?;
        }

        // Update branch head
        tx.execute(
            "INSERT OR REPLACE INTO branch_heads (branch_name, head_hash) VALUES (?1, ?2)",
            params![branch_name, commit.hash.as_str()],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        // Update legacy head
        tx.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            params!["head", commit.hash.as_str()],
        )
        .map_err(|e| OakError::Database(e.to_string()))?;

        tx.commit().map_err(|e| OakError::Database(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_blob_roundtrip() {
        let repo = SqliteRepository::in_memory().unwrap();
        let blob = Blob::from_str("hello world");

        repo.store_blob(&blob).unwrap();
        let retrieved = repo.get_blob(&blob.hash).unwrap().unwrap();

        assert_eq!(retrieved.hash, blob.hash);
        assert_eq!(retrieved.content, blob.content);
    }

    #[test]
    fn test_tree_put_walk_roundtrip() {
        let repo = SqliteRepository::in_memory().unwrap();
        // Store the blobs first (FK enforcement at sqlite level requires they exist).
        for content in ["a", "b", "c"] {
            repo.store_blob(&Blob::from_str(content)).unwrap();
        }

        let entries = vec![
            ManifestEntry {
                path: "src/lib.rs".to_string(),
                blob_hash: Blob::from_str("a").hash,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "src/bin/main.rs".to_string(),
                blob_hash: Blob::from_str("b").hash,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "README.md".to_string(),
                blob_hash: Blob::from_str("c").hash,
                mode: FileMode::Regular,
            },
        ];

        let root = repo.put_tree(entries.clone()).unwrap();
        assert!(repo.has_tree(&root).unwrap());

        let walked = repo.walk_tree(&root).unwrap();
        assert_eq!(walked.len(), 3);

        let manifest = Manifest::new(entries);
        let walked_manifest = Manifest::new(walked);
        assert_eq!(manifest.hash, walked_manifest.hash);
    }

    #[test]
    fn legacy_tree_entries_fallback() {
        // A tree written in the pre-0008 format (trees.content NULL + per-entry
        // tree_entries rows) must still read back correctly via get_tree's
        // fallback, identically to the new compressed-content format.
        let repo = SqliteRepository::in_memory().unwrap();
        let entries = vec![
            TreeEntry {
                name: "a.txt".to_string(),
                kind: TreeEntryKind::Blob,
                hash: Hash::from_hex(&"a".repeat(64)).unwrap(),
                mode: FileMode::Executable,
            },
            TreeEntry {
                name: "sub".to_string(),
                kind: TreeEntryKind::Tree,
                hash: Hash::from_hex(&"b".repeat(64)).unwrap(),
                mode: FileMode::Regular,
            },
        ];
        let tree = Tree::new(entries).unwrap();
        {
            let conn = repo.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO trees (hash) VALUES (?1)",
                rusqlite::params![tree.hash.as_str()],
            )
            .unwrap();
            for e in &tree.entries {
                let mode_str = match (e.kind, e.mode) {
                    (TreeEntryKind::Tree, _) => "tree",
                    (TreeEntryKind::Blob, FileMode::Regular) => "regular",
                    (TreeEntryKind::Blob, FileMode::Executable) => "executable",
                    (TreeEntryKind::Blob, FileMode::Symlink) => "symlink",
                };
                conn.execute(
                    "INSERT INTO tree_entries (tree_hash, name, kind, hash, mode) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![tree.hash.as_str(), e.name, e.kind.as_str(), e.hash.as_str(), mode_str],
                )
                .unwrap();
            }
        }
        let got = repo.get_tree(&tree.hash).unwrap().unwrap();
        assert_eq!(got.hash, tree.hash);
        assert_eq!(got.canonical_bytes(), tree.canonical_bytes());
    }

    #[test]
    fn compact_storage_backfills_drops_and_recompresses() {
        let repo = SqliteRepository::in_memory().unwrap();

        // A new-format tree via the normal write path (gets trees.content).
        repo.store_blob(&Blob::from_str("x")).unwrap();
        let new_root = repo
            .put_tree(vec![ManifestEntry {
                path: "dir/x.txt".to_string(),
                blob_hash: Blob::from_str("x").hash,
                mode: FileMode::Regular,
            }])
            .unwrap();

        // A legacy tree: content NULL + tree_entries rows.
        let legacy = Tree::new(vec![TreeEntry {
            name: "old.txt".to_string(),
            kind: TreeEntryKind::Blob,
            hash: Hash::from_hex(&"c".repeat(64)).unwrap(),
            mode: FileMode::Regular,
        }])
        .unwrap();
        // A legacy raw blob: codec 0, compressible.
        let legacy_blob = Blob::from_str(&"zzzz ".repeat(80));
        {
            let conn = repo.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO trees (hash) VALUES (?1)",
                rusqlite::params![legacy.hash.as_str()],
            )
            .unwrap();
            for e in &legacy.entries {
                conn.execute(
                    "INSERT INTO tree_entries (tree_hash, name, kind, hash, mode) VALUES (?1, ?2, ?3, ?4, 'regular')",
                    rusqlite::params![legacy.hash.as_str(), e.name, e.kind.as_str(), e.hash.as_str()],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO blobs (hash, content, size, codec) VALUES (?1, ?2, ?3, 0)",
                rusqlite::params![
                    legacy_blob.hash.as_str(),
                    legacy_blob.content,
                    legacy_blob.size as i64
                ],
            )
            .unwrap();
        }

        let stats = repo.compact_storage().unwrap();
        assert_eq!(stats.trees_backfilled, 1);
        assert!(stats.tree_entries_dropped);
        assert!(stats.blobs_recompressed >= 1);

        // tree_entries table is gone.
        {
            let conn = repo.conn.lock().unwrap();
            assert!(!table_exists(&conn, "tree_entries").unwrap());
        }

        // Both trees and both blobs still read correctly.
        assert!(repo.get_tree(&new_root).unwrap().is_some());
        let got_legacy = repo.get_tree(&legacy.hash).unwrap().unwrap();
        assert_eq!(got_legacy.canonical_bytes(), legacy.canonical_bytes());
        assert_eq!(
            repo.get_blob(&legacy_blob.hash).unwrap().unwrap().content,
            legacy_blob.content
        );

        // Idempotent: a second run is a no-op (plus a VACUUM).
        let stats2 = repo.compact_storage().unwrap();
        assert_eq!(stats2.trees_backfilled, 0);
        assert_eq!(stats2.blobs_recompressed, 0);
    }

    #[test]
    fn legacy_raw_blob_fallback() {
        // A pre-0007 blob row (raw content, codec 0) must read back as plaintext
        // even though new writes would have zstd-compressed it.
        let repo = SqliteRepository::in_memory().unwrap();
        let blob = Blob::from_str(&"hello world ".repeat(50)); // very compressible
        {
            let conn = repo.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO blobs (hash, content, size, codec) VALUES (?1, ?2, ?3, 0)",
                rusqlite::params![blob.hash.as_str(), blob.content, blob.size as i64],
            )
            .unwrap();
        }
        let got = repo.get_blob(&blob.hash).unwrap().unwrap();
        assert_eq!(got.content, blob.content);
    }

    #[test]
    fn new_blob_compresses_and_roundtrips() {
        // A compressible blob stored through the normal path is zstd-encoded
        // (codec 1) yet returns identical plaintext.
        let repo = SqliteRepository::in_memory().unwrap();
        let blob = Blob::from_str(&"abcabcabc ".repeat(100));
        repo.store_blob(&blob).unwrap();
        let codec: i64 = {
            let conn = repo.conn.lock().unwrap();
            conn.query_row(
                "SELECT codec FROM blobs WHERE hash = ?1",
                rusqlite::params![blob.hash.as_str()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(codec, 1, "compressible blob should store as zstd");
        let got = repo.get_blob(&blob.hash).unwrap().unwrap();
        assert_eq!(got.content, blob.content);
    }

    #[test]
    fn test_tree_subtree_dedup() {
        let repo = SqliteRepository::in_memory().unwrap();
        for content in ["x", "y"] {
            repo.store_blob(&Blob::from_str(content)).unwrap();
        }

        // Two roots that share the "shared/" subtree exactly.
        let entries_a = vec![
            ManifestEntry {
                path: "shared/x.txt".to_string(),
                blob_hash: Blob::from_str("x").hash,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "shared/y.txt".to_string(),
                blob_hash: Blob::from_str("y").hash,
                mode: FileMode::Regular,
            },
        ];
        let entries_b = vec![
            ManifestEntry {
                path: "shared/x.txt".to_string(),
                blob_hash: Blob::from_str("x").hash,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "shared/y.txt".to_string(),
                blob_hash: Blob::from_str("y").hash,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "extra.txt".to_string(),
                blob_hash: Blob::from_str("x").hash,
                mode: FileMode::Regular,
            },
        ];

        let root_a = repo.put_tree(entries_a).unwrap();
        let root_b = repo.put_tree(entries_b).unwrap();
        assert_ne!(root_a, root_b);

        // The "shared" subtree should be the same object for both roots.
        let shared_a = repo.subtree_at_path(&root_a, "shared").unwrap().unwrap();
        let shared_b = repo.subtree_at_path(&root_b, "shared").unwrap().unwrap();
        assert_eq!(shared_a, shared_b);
    }

    #[test]
    fn test_tree_resolve_path() {
        let repo = SqliteRepository::in_memory().unwrap();
        repo.store_blob(&Blob::from_str("file")).unwrap();
        let entries = vec![ManifestEntry {
            path: "a/b/c.txt".to_string(),
            blob_hash: Blob::from_str("file").hash,
            mode: FileMode::Regular,
        }];
        let root = repo.put_tree(entries).unwrap();

        let blob = repo.resolve_path_in_tree(&root, "a/b/c.txt").unwrap();
        assert!(blob.is_some());
        assert_eq!(blob.unwrap().kind, TreeEntryKind::Blob);

        let dir = repo.resolve_path_in_tree(&root, "a/b").unwrap();
        assert_eq!(dir.unwrap().kind, TreeEntryKind::Tree);

        let missing = repo.resolve_path_in_tree(&root, "a/zzz.txt").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_tree_put_idempotent() {
        let repo = SqliteRepository::in_memory().unwrap();
        repo.store_blob(&Blob::from_str("c")).unwrap();
        let entries = vec![ManifestEntry {
            path: "x.txt".to_string(),
            blob_hash: Blob::from_str("c").hash,
            mode: FileMode::Regular,
        }];
        let r1 = repo.put_tree(entries.clone()).unwrap();
        let r2 = repo.put_tree(entries).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_manifest_roundtrip() {
        let repo = SqliteRepository::in_memory().unwrap();

        let blob = Blob::from_str("content");
        repo.store_blob(&blob).unwrap();

        let manifest = Manifest::new(vec![ManifestEntry {
            path: "test.txt".to_string(),
            blob_hash: blob.hash.clone(),
            mode: FileMode::Regular,
        }]);

        repo.store_manifest(&manifest).unwrap();
        let retrieved = repo.get_manifest(&manifest.hash).unwrap().unwrap();

        assert_eq!(retrieved.hash, manifest.hash);
        assert_eq!(retrieved.entries.len(), 1);
        assert_eq!(retrieved.entries[0].path, "test.txt");
    }

    #[test]
    fn test_branch_roundtrip() {
        let repo = SqliteRepository::in_memory().unwrap();

        let br = Branch::new("feature-x".to_string(), Some("A feature".to_string()), None);
        repo.store_branch(&br).unwrap();

        let retrieved = repo.get_branch("feature-x").unwrap().unwrap();
        assert_eq!(retrieved.name, "feature-x");
        assert_eq!(retrieved.description, Some("A feature".to_string()));
        assert_eq!(retrieved.status, BranchStatus::Open);
    }

    #[test]
    fn test_commit_roundtrip() {
        let repo = SqliteRepository::in_memory().unwrap();

        let blob = Blob::from_str("content");
        repo.store_blob(&blob).unwrap();

        let manifest = Manifest::new(vec![ManifestEntry {
            path: "test.txt".to_string(),
            blob_hash: blob.hash.clone(),
            mode: FileMode::Regular,
        }]);
        repo.store_manifest(&manifest).unwrap();

        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        let commit = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "Test Author".to_string(),
            Some("Initial commit".to_string()),
            vec![FileChange {
                path: "test.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob.hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: None,
            }],
        )
        .unwrap();

        repo.store_commit(&commit).unwrap();
        let retrieved = repo.get_commit(&commit.hash).unwrap().unwrap();

        assert_eq!(retrieved.hash, commit.hash);
        assert_eq!(retrieved.branch_name, "main");
        assert_eq!(retrieved.message, Some("Initial commit".to_string()));
        assert_eq!(retrieved.files.len(), 1);
    }

    #[test]
    fn test_branch_head() {
        let repo = SqliteRepository::in_memory().unwrap();

        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        assert!(repo.get_branch_head("main").unwrap().is_none());

        let blob = Blob::new(b"test".to_vec());
        repo.store_blob(&blob).unwrap();
        let manifest = Manifest::new(vec![ManifestEntry {
            path: "test.txt".to_string(),
            blob_hash: blob.hash.clone(),
            mode: FileMode::Regular,
        }]);
        repo.store_manifest(&manifest).unwrap();

        let commit = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "test".to_string(),
            Some("test commit".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();

        repo.set_branch_head("main", &commit.hash).unwrap();

        let retrieved = repo.get_branch_head("main").unwrap().unwrap();
        assert_eq!(retrieved, commit.hash);
    }

    #[test]
    fn test_list_branches() {
        let repo = SqliteRepository::in_memory().unwrap();

        let br1 = Branch::new("main".to_string(), None, None);
        let br2 = Branch::new(
            "feature".to_string(),
            Some("feature branch".to_string()),
            Some("main".to_string()),
        );
        repo.store_branch(&br1).unwrap();
        repo.store_branch(&br2).unwrap();

        let all = repo.list_branches().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_commits_for_branch() {
        let repo = SqliteRepository::in_memory().unwrap();

        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();

        let commit1 = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "author".to_string(),
            Some("commit 1".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&commit1).unwrap();

        let commit2 = Commit::new(
            "main".to_string(),
            Some(commit1.hash.clone()),
            None,
            manifest.hash.clone(),
            "author".to_string(),
            Some("commit 2".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&commit2).unwrap();

        let commits = repo.get_commits_for_branch("main").unwrap();
        assert_eq!(commits.len(), 2);
    }

    #[test]
    fn test_count_commits_for_branch() {
        let repo = SqliteRepository::in_memory().unwrap();

        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();

        // A branch with no commits of its own (freshly seeded onto a parent)
        // counts zero — the unmerged-work baseline.
        assert_eq!(repo.count_commits_for_branch("feature").unwrap(), 0);

        // One commit on `main`, two authored on `feature`. The count is keyed
        // strictly off the `branch_name` column, so `feature` reports 2
        // regardless of `main`'s history — exactly the "commits not yet merged
        // into the parent" signal.
        let base = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "author".to_string(),
            None,
            vec![],
        )
        .unwrap();
        repo.store_commit(&base).unwrap();

        let feat1 = Commit::new(
            "feature".to_string(),
            Some(base.hash.clone()),
            None,
            manifest.hash.clone(),
            "author".to_string(),
            None,
            vec![],
        )
        .unwrap();
        repo.store_commit(&feat1).unwrap();

        let feat2 = Commit::new(
            "feature".to_string(),
            Some(feat1.hash.clone()),
            None,
            manifest.hash.clone(),
            "author".to_string(),
            None,
            vec![],
        )
        .unwrap();
        repo.store_commit(&feat2).unwrap();

        assert_eq!(repo.count_commits_for_branch("feature").unwrap(), 2);
        assert_eq!(repo.count_commits_for_branch("main").unwrap(), 1);
        // Matches the materializing path it optimizes.
        assert_eq!(
            repo.count_commits_for_branch("feature").unwrap(),
            repo.get_commits_for_branch("feature").unwrap().len()
        );
    }

    #[test]
    fn test_merge_child_exists() {
        let repo = SqliteRepository::in_memory().unwrap();

        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();

        let feature = Commit::new(
            "feature".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "author".to_string(),
            None,
            vec![],
        )
        .unwrap();
        repo.store_commit(&feature).unwrap();

        assert!(!repo.merge_child_exists(&feature.hash).unwrap());

        let main_merge = Commit::new(
            "main".to_string(),
            None,
            Some(feature.hash.clone()),
            manifest.hash.clone(),
            "author".to_string(),
            Some("merged feature".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&main_merge).unwrap();

        assert!(repo.merge_child_exists(&feature.hash).unwrap());
    }

    #[test]
    fn test_metadata() {
        let repo = SqliteRepository::in_memory().unwrap();

        assert!(repo.get_head().unwrap().is_none());

        let hash = Hash("a".repeat(64));
        repo.set_head(&hash).unwrap();

        let retrieved = repo.get_head().unwrap().unwrap();
        assert_eq!(retrieved, hash);
    }

    #[test]
    fn test_current_branch() {
        let repo = SqliteRepository::in_memory().unwrap();

        assert!(repo.get_current_branch_name().unwrap().is_none());

        repo.set_current_branch("main").unwrap();
        let name = repo.get_current_branch_name().unwrap().unwrap();
        assert_eq!(name, "main");
    }

    #[test]
    fn test_tag_roundtrip() {
        let repo = SqliteRepository::in_memory().unwrap();

        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();

        let commit = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "author".to_string(),
            Some("commit".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();

        repo.create_tag("v1.0", &commit.hash).unwrap();

        let tags = repo.list_tags().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "v1.0");
        assert_eq!(tags[0].commit_hash, commit.hash);

        repo.delete_tag("v1.0").unwrap();
        let tags = repo.list_tags().unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_branch_description_update() {
        let repo = SqliteRepository::in_memory().unwrap();

        let br = Branch::new("main".to_string(), Some("old desc".to_string()), None);
        repo.store_branch(&br).unwrap();

        repo.update_branch_description("main", "new desc").unwrap();

        let updated = repo.get_branch("main").unwrap().unwrap();
        assert_eq!(updated.description, Some("new desc".to_string()));
    }

    #[test]
    fn test_get_commits_since_edge_cases() {
        let repo = SqliteRepository::in_memory().unwrap();

        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();

        // No commits - get_commits_since should return empty
        let commits = repo.get_commits_since("main", None).unwrap();
        assert!(commits.is_empty());

        // With a non-existent since hash and no commits — returns empty (no commits on branch)
        let commits = repo
            .get_commits_since("main", Some(&Hash("nonexistent".to_string())))
            .unwrap();
        assert!(commits.is_empty());
    }

    #[test]
    fn test_get_commits_since_cross_branch_hash_returns_all() {
        // When since_hash belongs to a different branch (not found in this
        // branch's commits), get_commits_since should return ALL commits
        // for this branch, since they are all new from the caller's view.
        let repo = SqliteRepository::in_memory().unwrap();
        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();
        repo.store_branch(&Branch::new("default".to_string(), None, None))
            .unwrap();
        repo.store_branch(&Branch::new(
            "update".to_string(),
            None,
            Some("default".to_string()),
        ))
        .unwrap();

        // Commit on "default"
        let c_default = Commit::new(
            "default".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("default commit".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&c_default).unwrap();

        // Commits on "update"
        let c_update1 = Commit::new(
            "update".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("update commit 1".to_string()),
            vec![],
        )
        .unwrap();
        let c_update2 = Commit::new(
            "update".to_string(),
            Some(c_update1.hash.clone()),
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("update commit 2".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&c_update1).unwrap();
        repo.store_commit(&c_update2).unwrap();

        // get_commits_since("update", default_branch_head) should return
        // ALL "update" commits since the hash doesn't belong to "update"
        let since = repo
            .get_commits_since("update", Some(&c_default.hash))
            .unwrap();
        assert_eq!(
            since.len(),
            2,
            "should return all update commits when since_hash is from another branch"
        );
        assert_eq!(since[0].message, Some("update commit 1".to_string()));
        assert_eq!(since[1].message, Some("update commit 2".to_string()));
    }

    // ============================================================
    // Blob edge cases
    // ============================================================

    #[test]
    fn test_has_blob() {
        let repo = SqliteRepository::in_memory().unwrap();
        let blob = Blob::from_str("test content");

        assert!(!repo.has_blob(&blob.hash).unwrap());
        repo.store_blob(&blob).unwrap();
        assert!(repo.has_blob(&blob.hash).unwrap());
    }

    #[test]
    fn test_get_nonexistent_blob() {
        let repo = SqliteRepository::in_memory().unwrap();
        let result = repo.get_blob(&Hash("nonexistent".to_string())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_store_blob_idempotent() {
        let repo = SqliteRepository::in_memory().unwrap();
        let blob = Blob::from_str("same content");

        repo.store_blob(&blob).unwrap();
        repo.store_blob(&blob).unwrap(); // Should not error (INSERT OR IGNORE)

        let retrieved = repo.get_blob(&blob.hash).unwrap().unwrap();
        assert_eq!(retrieved.content, blob.content);
    }

    #[test]
    fn test_blob_empty_content() {
        let repo = SqliteRepository::in_memory().unwrap();
        let blob = Blob::new(Vec::new());

        repo.store_blob(&blob).unwrap();
        let retrieved = repo.get_blob(&blob.hash).unwrap().unwrap();
        assert!(retrieved.content.is_empty());
        assert_eq!(retrieved.size, 0);
    }

    #[test]
    fn test_blob_binary_content() {
        let repo = SqliteRepository::in_memory().unwrap();
        let binary_data: Vec<u8> = (0..=255).collect();
        let blob = Blob::new(binary_data.clone());

        repo.store_blob(&blob).unwrap();
        let retrieved = repo.get_blob(&blob.hash).unwrap().unwrap();
        assert_eq!(retrieved.content, binary_data);
    }

    // ============================================================
    // Manifest edge cases
    // ============================================================

    #[test]
    fn test_get_nonexistent_manifest() {
        let repo = SqliteRepository::in_memory().unwrap();
        let result = repo.get_manifest(&Hash("nonexistent".to_string())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_manifest_with_multiple_entries() {
        let repo = SqliteRepository::in_memory().unwrap();

        let blob1 = Blob::from_str("content1");
        let blob2 = Blob::from_str("content2");
        repo.store_blob(&blob1).unwrap();
        repo.store_blob(&blob2).unwrap();

        let manifest = Manifest::new(vec![
            ManifestEntry {
                path: "src/main.rs".to_string(),
                blob_hash: blob1.hash.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "scripts/build.sh".to_string(),
                blob_hash: blob2.hash.clone(),
                mode: FileMode::Executable,
            },
        ]);

        repo.store_manifest(&manifest).unwrap();
        let retrieved = repo.get_manifest(&manifest.hash).unwrap().unwrap();

        assert_eq!(retrieved.entries.len(), 2);
        let paths: Vec<&str> = retrieved.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"scripts/build.sh"));

        // Check modes are preserved
        let exec_entry = retrieved
            .entries
            .iter()
            .find(|e| e.path == "scripts/build.sh")
            .unwrap();
        assert_eq!(exec_entry.mode, FileMode::Executable);
    }

    #[test]
    fn test_store_manifest_idempotent() {
        let repo = SqliteRepository::in_memory().unwrap();
        let manifest = Manifest::empty();

        repo.store_manifest(&manifest).unwrap();
        repo.store_manifest(&manifest).unwrap(); // Should not error
    }

    // ============================================================
    // Branch edge cases
    // ============================================================

    #[test]
    fn test_get_nonexistent_branch() {
        let repo = SqliteRepository::in_memory().unwrap();
        let result = repo.get_branch("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_branch_status_update() {
        let repo = SqliteRepository::in_memory().unwrap();
        let br = Branch::new("feature".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        assert_eq!(
            repo.get_branch("feature").unwrap().unwrap().status,
            BranchStatus::Open
        );

        repo.update_branch_status("feature", BranchStatus::Closed)
            .unwrap();
        assert_eq!(
            repo.get_branch("feature").unwrap().unwrap().status,
            BranchStatus::Closed
        );

        repo.update_branch_status("feature", BranchStatus::Open)
            .unwrap();
        assert_eq!(
            repo.get_branch("feature").unwrap().unwrap().status,
            BranchStatus::Open
        );
    }

    #[test]
    fn test_branch_with_parent() {
        let repo = SqliteRepository::in_memory().unwrap();

        let main = Branch::new("main".to_string(), None, None);
        let feature = Branch::new(
            "feature".to_string(),
            Some("my feature".to_string()),
            Some("main".to_string()),
        );
        repo.store_branch(&main).unwrap();
        repo.store_branch(&feature).unwrap();

        let retrieved = repo.get_branch("feature").unwrap().unwrap();
        assert_eq!(retrieved.parent_branch, Some("main".to_string()));
        assert_eq!(retrieved.description, Some("my feature".to_string()));
    }

    #[test]
    fn test_get_branch_head_nonexistent() {
        let repo = SqliteRepository::in_memory().unwrap();
        let result = repo.get_branch_head("nonexistent").unwrap();
        assert!(result.is_none());
    }

    // ============================================================
    // Commit edge cases
    // ============================================================

    #[test]
    fn test_has_commit() {
        let repo = SqliteRepository::in_memory().unwrap();
        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();
        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        let commit = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "author".to_string(),
            Some("test".to_string()),
            vec![],
        )
        .unwrap();

        assert!(!repo.has_commit(&commit.hash).unwrap());
        repo.store_commit(&commit).unwrap();
        assert!(repo.has_commit(&commit.hash).unwrap());
    }

    #[test]
    fn test_get_nonexistent_commit() {
        let repo = SqliteRepository::in_memory().unwrap();
        let result = repo.get_commit(&Hash("nonexistent".to_string())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_commits_since_with_valid_hash() {
        let repo = SqliteRepository::in_memory().unwrap();
        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();
        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        let c1 = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("c1".to_string()),
            vec![],
        )
        .unwrap();
        let c2 = Commit::new(
            "main".to_string(),
            Some(c1.hash.clone()),
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("c2".to_string()),
            vec![],
        )
        .unwrap();
        let c3 = Commit::new(
            "main".to_string(),
            Some(c2.hash.clone()),
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("c3".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&c1).unwrap();
        repo.store_commit(&c2).unwrap();
        repo.store_commit(&c3).unwrap();

        // Get commits since c1 should return c2 and c3
        let since = repo.get_commits_since("main", Some(&c1.hash)).unwrap();
        assert_eq!(since.len(), 2);
        assert_eq!(since[0].message, Some("c2".to_string()));
        assert_eq!(since[1].message, Some("c3".to_string()));

        // Get commits since c2 should return only c3
        let since = repo.get_commits_since("main", Some(&c2.hash)).unwrap();
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].message, Some("c3".to_string()));

        // Get commits since c3 should return empty
        let since = repo.get_commits_since("main", Some(&c3.hash)).unwrap();
        assert!(since.is_empty());
    }

    #[test]
    fn test_get_all_commits() {
        let repo = SqliteRepository::in_memory().unwrap();
        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.store_branch(&Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();

        let c1 = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("main commit".to_string()),
            vec![],
        )
        .unwrap();
        let c2 = Commit::new(
            "feature".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "a".to_string(),
            Some("feature commit".to_string()),
            vec![],
        )
        .unwrap();
        repo.store_commit(&c1).unwrap();
        repo.store_commit(&c2).unwrap();

        let all = repo.get_all_commits().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_commit_with_file_changes() {
        let repo = SqliteRepository::in_memory().unwrap();
        let manifest = Manifest::empty();
        repo.store_manifest(&manifest).unwrap();
        let br = Branch::new("main".to_string(), None, None);
        repo.store_branch(&br).unwrap();

        let blob_hash = Hash("abc123".repeat(11)[..64].to_string());
        let commit = Commit::new(
            "main".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "author".to_string(),
            Some("with changes".to_string()),
            vec![
                FileChange {
                    path: "added.txt".to_string(),
                    change_type: ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(blob_hash.clone()),
                    old_path: None,
                    old_mode: None,
                    new_mode: None,
                },
                FileChange {
                    path: "modified.txt".to_string(),
                    change_type: ChangeType::Modified,
                    old_blob_hash: Some(blob_hash.clone()),
                    new_blob_hash: Some(blob_hash.clone()),
                    old_path: None,
                    old_mode: None,
                    new_mode: None,
                },
                FileChange {
                    path: "deleted.txt".to_string(),
                    change_type: ChangeType::Deleted,
                    old_blob_hash: Some(blob_hash.clone()),
                    new_blob_hash: None,
                    old_path: None,
                    old_mode: None,
                    new_mode: None,
                },
            ],
        )
        .unwrap();

        repo.store_commit(&commit).unwrap();
        let retrieved = repo.get_commit(&commit.hash).unwrap().unwrap();

        assert_eq!(retrieved.files.len(), 3);
        assert!(retrieved
            .files
            .iter()
            .any(|f| f.path == "added.txt" && f.change_type == ChangeType::Added));
        assert!(retrieved
            .files
            .iter()
            .any(|f| f.path == "modified.txt" && f.change_type == ChangeType::Modified));
        assert!(retrieved
            .files
            .iter()
            .any(|f| f.path == "deleted.txt" && f.change_type == ChangeType::Deleted));
    }

    // ============================================================
    // Chunk operations
    // ============================================================

    #[test]
    fn test_chunk_roundtrip() {
        let repo = SqliteRepository::in_memory().unwrap();
        let hash = Hash("chunkhash".repeat(8)[..64].to_string());
        let data = b"chunk content here";

        assert!(!repo.has_chunk(&hash).unwrap());
        repo.store_chunk(&hash, data).unwrap();
        assert!(repo.has_chunk(&hash).unwrap());

        let retrieved = repo.get_chunk(&hash).unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_get_nonexistent_chunk() {
        let repo = SqliteRepository::in_memory().unwrap();
        let result = repo.get_chunk(&Hash("nonexistent".to_string())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_blob_chunks_mapping() {
        let repo = SqliteRepository::in_memory().unwrap();
        let blob_hash = Hash("blobhash".repeat(8)[..64].to_string());
        let chunk1_hash = Hash("chunk1".repeat(11)[..64].to_string());
        let chunk2_hash = Hash("chunk2".repeat(11)[..64].to_string());

        // Store the actual chunks first (FK constraint)
        repo.store_chunk(&chunk1_hash, b"chunk1data").unwrap();
        repo.store_chunk(&chunk2_hash, b"chunk2data").unwrap();

        let chunks = vec![
            ChunkInfo {
                hash: chunk1_hash,
                offset: 0,
                length: 1024,
            },
            ChunkInfo {
                hash: chunk2_hash,
                offset: 1024,
                length: 512,
            },
        ];

        // No mapping initially
        assert!(repo.get_blob_chunks(&blob_hash).unwrap().is_none());

        repo.store_blob_chunks(&blob_hash, &chunks).unwrap();

        let retrieved = repo.get_blob_chunks(&blob_hash).unwrap().unwrap();
        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0].offset, 0);
        assert_eq!(retrieved[0].length, 1024);
        assert_eq!(retrieved[1].offset, 1024);
        assert_eq!(retrieved[1].length, 512);
    }

    // ============================================================
    // Metadata edge cases
    // ============================================================

    #[test]
    fn test_metadata_overwrite() {
        let repo = SqliteRepository::in_memory().unwrap();

        repo.set_metadata(MetadataKey::RemoteUrl, "http://old.example.com")
            .unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, "http://new.example.com")
            .unwrap();

        let val = repo.get_metadata(MetadataKey::RemoteUrl).unwrap().unwrap();
        assert_eq!(val, "http://new.example.com");
    }

    #[test]
    fn test_metadata_nonexistent_key() {
        let repo = SqliteRepository::in_memory().unwrap();
        let result = repo.get_metadata(MetadataKey::ApiKey).unwrap();
        assert!(result.is_none());
    }

    // ============================================================
    // BulkImporter
    // ============================================================

    #[test]
    fn test_bulk_importer_roundtrip() {
        // Importer needs a file-backed connection — in-memory rejects.
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("oak.db");
        let repo = SqliteRepository::open(&db_path).unwrap();
        let branch = Branch::new("main".to_string(), None, None);
        repo.store_branch(&branch).unwrap();

        let mut importer = repo.bulk_importer(2).unwrap();
        // Two commits, each with the same single-file manifest. Forces the
        // tree cache to short-circuit the second commit's subtree inserts.
        let blob_hash = importer.put_blob(b"hello".to_vec()).unwrap();
        let entries = vec![ManifestEntry {
            path: "a.txt".to_string(),
            blob_hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }];
        let manifest_hash = importer.put_tree(entries.clone()).unwrap();

        let ts = chrono::DateTime::from_timestamp(1_000_000, 0).unwrap();
        let c1 = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            manifest_hash.clone(),
            "tester <t@example.com>".to_string(),
            None,
            Vec::new(),
            ts,
        )
        .unwrap();
        importer.store_commit(&c1).unwrap();
        // Re-using the same entries on commit 2 exercises the in-memory
        // tree cache: build_tree returns the same subtree hashes, which the
        // importer should recognize without re-inserting.
        let manifest_hash2 = importer.put_tree(entries).unwrap();
        assert_eq!(manifest_hash, manifest_hash2);
        let c2 = Commit::with_timestamp(
            "main".to_string(),
            Some(c1.hash.clone()),
            None,
            manifest_hash2,
            "tester <t@example.com>".to_string(),
            None,
            Vec::new(),
            ts,
        )
        .unwrap();
        importer.store_commit(&c2).unwrap();
        importer.finish().unwrap();

        // Both commits durable, blob readable through the normal repo.
        assert!(repo.has_commit(&c1.hash).unwrap());
        assert!(repo.has_commit(&c2.hash).unwrap());
        assert!(repo.has_blob(&blob_hash).unwrap());
        let manifest = repo.get_manifest(&manifest_hash).unwrap().unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].path, "a.txt");
    }

    #[test]
    fn test_bulk_importer_drop_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("oak.db");
        let repo = SqliteRepository::open(&db_path).unwrap();

        let commit_hash = {
            let mut importer = repo.bulk_importer(100).unwrap();
            let blob_hash = importer.put_blob(b"ephemeral".to_vec()).unwrap();
            let entries = vec![ManifestEntry {
                path: "x.txt".to_string(),
                blob_hash,
                mode: FileMode::Regular,
            }];
            let manifest_hash = importer.put_tree(entries).unwrap();
            let c = Commit::with_timestamp(
                "main".to_string(),
                None,
                None,
                manifest_hash,
                "tester <t@example.com>".to_string(),
                None,
                Vec::new(),
                chrono::DateTime::from_timestamp(0, 0).unwrap(),
            )
            .unwrap();
            importer.store_commit(&c).unwrap();
            c.hash
            // importer dropped here without finish() → outer tx rolled back.
        };

        // The unflushed commit must not be visible.
        assert!(!repo.has_commit(&commit_hash).unwrap());
    }

    #[test]
    fn test_bulk_txn_commit_persists() {
        // Writes issued inside a bulk transaction are durable after commit —
        // this is the clone/pull ingest path (store_chunk et al. join the
        // open BEGIN rather than auto-committing per statement).
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("oak.db");
        let repo = SqliteRepository::open(&db_path).unwrap();

        let a = Hash("chunk_a".to_string());
        let b = Hash("chunk_b".to_string());
        repo.bulk_begin().unwrap();
        repo.store_chunk(&a, b"aaaa").unwrap();
        repo.store_chunk(&b, b"bbbb").unwrap();
        repo.bulk_commit().unwrap();

        assert!(repo.has_chunk(&a).unwrap());
        assert!(repo.has_chunk(&b).unwrap());
    }

    #[test]
    fn test_bulk_txn_rollback_discards() {
        // bulk_rollback (the BulkTxn guard's drop path) must discard every
        // write made since bulk_begin, so a failed import leaves nothing
        // half-applied.
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("oak.db");
        let repo = SqliteRepository::open(&db_path).unwrap();

        let a = Hash("chunk_a".to_string());
        repo.bulk_begin().unwrap();
        repo.store_chunk(&a, b"aaaa").unwrap();
        repo.bulk_rollback();

        assert!(!repo.has_chunk(&a).unwrap());
        // Connection is usable again afterwards (rollback restored a clean
        // auto-commit state, not a dangling transaction).
        repo.store_chunk(&a, b"aaaa").unwrap();
        assert!(repo.has_chunk(&a).unwrap());
    }

    #[test]
    fn test_bulk_txn_flush_checkpoints_then_rollback() {
        // bulk_flush commits the batch so far and opens the next one. A write
        // before the flush survives; a write after it (with no further
        // commit) is rolled back. This is what bounds WAL growth mid-import
        // without losing already-flushed objects.
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("oak.db");
        let repo = SqliteRepository::open(&db_path).unwrap();

        let before = Hash("before_flush".to_string());
        let after = Hash("after_flush".to_string());
        repo.bulk_begin().unwrap();
        repo.store_chunk(&before, b"keep").unwrap();
        repo.bulk_flush().unwrap();
        repo.store_chunk(&after, b"drop").unwrap();
        repo.bulk_rollback();

        assert!(repo.has_chunk(&before).unwrap());
        assert!(!repo.has_chunk(&after).unwrap());
    }
}
