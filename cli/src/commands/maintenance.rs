//! `oak maintenance` — local database upkeep.
//!
//! Currently a single operation, `compact`, which converts the local SQLite
//! database to the compact on-disk storage format and reclaims the freed space:
//!
//!   - backfills `trees.content` for any tree still stored as `tree_entries`
//!     rows (the pre-0008 format), then drops the `tree_entries` table;
//!   - zstd-compresses any blob still stored raw (pre-0007);
//!   - `VACUUM`s to shrink the file on disk.
//!
//! On large/long-history repos the `tree_entries` table and its indexes
//! dominated the database (often the large majority of it), so this can shrink
//! the file dramatically. It only touches *storage representation* — every
//! object keeps its content-addressed hash — so it's safe to run at any time.
//!
//! `VACUUM` rewrites the database, so it needs transient free disk space roughly
//! equal to the final (post-compaction) size.

use std::path::Path;

use oak_core::{OakError, Result, SqliteRepository};

use crate::resolve::Backend;

/// Run `oak maintenance compact` against the repo containing `path`.
pub fn compact(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    if ctx.backend != Backend::Sqlite {
        return Err(OakError::Database(
            "compaction only applies to native Oak repositories (.oak/oak.db)".to_string(),
        ));
    }

    let db_path = ctx.db_path()?;
    let size_before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    crate::output::print_line(&format!("Compacting {} …", db_path.display()));
    let repo = SqliteRepository::open(&db_path)?;
    let stats = repo.compact_storage()?;
    drop(repo);

    let size_after = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    crate::output::print_line(&format!("  trees backfilled:   {}", stats.trees_backfilled));
    crate::output::print_line(&format!(
        "  tree_entries table: {}",
        if stats.tree_entries_dropped {
            "dropped"
        } else {
            "kept (some trees not yet converted)"
        }
    ));
    crate::output::print_line(&format!(
        "  blobs recompressed: {}",
        stats.blobs_recompressed
    ));
    crate::output::print_line(&format!(
        "  size: {} → {} ({})",
        human_bytes(size_before),
        human_bytes(size_after),
        if size_after < size_before {
            format!("-{}", human_bytes(size_before - size_after))
        } else {
            "no change".to_string()
        }
    ));
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
