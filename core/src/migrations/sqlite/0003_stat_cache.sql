-- Working-tree stat cache. One row per file last seen by `scan_working_dir`,
-- letting `oak status`/`oak commit` skip re-hashing files whose (mtime, size)
-- are unchanged. Local-only; never synced. Safe to wipe (just forces a rehash).
CREATE TABLE IF NOT EXISTS stat_cache (
    path      TEXT    PRIMARY KEY,
    mtime_ns  INTEGER NOT NULL,
    size      INTEGER NOT NULL,
    blob_hash TEXT    NOT NULL
);
