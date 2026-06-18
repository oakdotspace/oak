-- Per-blob storage codec for at-rest compression of blobs.content.
--   0 = raw (uncompressed)  — the implicit format for every pre-existing row
--   1 = zstd
-- The `size` column remains the LOGICAL (uncompressed) blob size, so stats and
-- quotas are unaffected; only the stored `content` bytes change. Blobs stay
-- content-addressed by the hash of their uncompressed bytes. Existing rows keep
-- codec 0 until the `oak maintenance compact` command recompresses them.
ALTER TABLE blobs ADD COLUMN codec INTEGER NOT NULL DEFAULT 0;
