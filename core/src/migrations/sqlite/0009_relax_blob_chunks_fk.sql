-- Drop the FK on `blob_chunks.chunk_hash`, making a blob→chunk mapping
-- storable before the chunk bytes themselves are present locally.
--
-- Why: `oak mount` learns every blob's chunk refs up front (the `/blobs/info`
-- call it already makes to populate sizes returns them), and persisting those
-- refs lets a later cold read skip a second `/blobs/info` round-trip and go
-- straight to `/chunks/download` — roughly halving cold open latency. But the
-- mount caches refs *without* downloading the chunk bytes, so the FK
-- `chunk_hash REFERENCES chunks(hash)` rejects the insert with "FOREIGN KEY
-- constraint failed". The mapping is content-addressed metadata; the chunk
-- bytes are validated at reassembly time (`get_chunk` errors if a chunk is
-- missing), so the FK is a redundant safety net here.
--
-- SQLite can't ALTER a column to drop a FK, so we rebuild the table. Same
-- PRAGMA gating and "new_X / drop X / rename new_X -> X" pattern as
-- 0002_relax_commits_parent_hash_fk.

PRAGMA foreign_keys = OFF;
PRAGMA legacy_alter_table = ON;

BEGIN;

CREATE TABLE new_blob_chunks (
    blob_hash TEXT NOT NULL,
    chunk_hash TEXT NOT NULL,
    chunk_index INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (blob_hash, chunk_index)
);
INSERT INTO new_blob_chunks (blob_hash, chunk_hash, chunk_index, offset, size)
    SELECT blob_hash, chunk_hash, chunk_index, offset, size FROM blob_chunks;
DROP TABLE blob_chunks;
ALTER TABLE new_blob_chunks RENAME TO blob_chunks;

COMMIT;

PRAGMA legacy_alter_table = OFF;
PRAGMA foreign_keys = ON;
