-- Drop the FK on `tags.commit_hash`, making a tag's commit a soft pointer
-- like `commits.parent_hash` (0002).
--
-- Why: `/pull?tags=v1` sends every tag on the server, but a shallow clone only
-- holds main's head. Any tag on an older commit then fails the whole clone with
-- "FOREIGN KEY constraint failed", and clone cleans up the directory. Tags are
-- a metadata cache of the server's snapshot, and `LocalRelease::commit_hash` is
-- documented as possibly absent locally; readers that need the commit look it
-- up and skip the tag when it is missing (`export_tags`).
--
-- Unlike 0002 and 0009 this does not touch `PRAGMA foreign_keys`. No table
-- references `tags`, so the rebuild can't trip enforcement, and leaving the
-- pragma alone keeps a relaxed connection (`open_relaxed`) relaxed.
BEGIN;

CREATE TABLE new_tags (
    name TEXT PRIMARY KEY,
    commit_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    title TEXT,
    notes TEXT,
    is_prerelease INTEGER NOT NULL DEFAULT 0,
    finalized INTEGER NOT NULL DEFAULT 0,
    published_at TEXT,
    created_by TEXT NOT NULL DEFAULT '',
    revision INTEGER NOT NULL DEFAULT 0,
    synced INTEGER NOT NULL DEFAULT 0
);
INSERT INTO new_tags (name, commit_hash, created_at, title, notes, is_prerelease,
                      finalized, published_at, created_by, revision, synced)
    SELECT name, commit_hash, created_at, title, notes, is_prerelease,
           finalized, published_at, created_by, revision, synced FROM tags;
DROP TABLE tags;
ALTER TABLE new_tags RENAME TO tags;

COMMIT;
