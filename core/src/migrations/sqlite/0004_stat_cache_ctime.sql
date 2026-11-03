-- Add a ctime (inode change time) column to the working-tree stat cache.
--
-- The cache previously validated a cached blob hash on (mtime_ns, size) alone.
-- That pair is forgeable: an edit that keeps the file's size and lands on the
-- same mtime — a same-second/coarse-resolution clock, two rapid edits, or an
-- mtime explicitly reset by tooling (`touch`, `utime`, a checkout) — looks
-- "unchanged" and the real edit is silently missed by `oak status`, `oak diff`,
-- AND `oak commit` (which then records stale content). Adding ctime closes
-- this: any content write OR an mtime reset bumps ctime, and ctime can't be set
-- backwards from userspace, so a stale-but-same-(mtime,size) file now misses
-- the cache and gets re-hashed.
--
-- The stat cache is a pure local performance cache (never synced, safe to
-- wipe), so we just drop and recreate it. Existing rows are discarded; the next
-- scan re-hashes every file once and repopulates with ctime. ctime_ns is 0 on
-- platforms that can't report a POSIX ctime (e.g. Windows), where the hit check
-- degrades to the previous (mtime, size) behavior.
DROP TABLE IF EXISTS stat_cache;
CREATE TABLE IF NOT EXISTS stat_cache (
    path      TEXT    PRIMARY KEY,
    mtime_ns  INTEGER NOT NULL,
    ctime_ns  INTEGER NOT NULL DEFAULT 0,
    size      INTEGER NOT NULL,
    blob_hash TEXT    NOT NULL
);
