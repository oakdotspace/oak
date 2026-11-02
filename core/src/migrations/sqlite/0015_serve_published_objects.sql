-- `oak serve`'s set of immutable objects made reachable by a branch head.
--
-- Replaces the `serve_published_closure` metadata JSON value, which every
-- push, pull and chunk fetch parsed in full and every publish rewrote in full
-- (O(repo) per request, inside the per-repo writer lock). Membership is now a
-- primary-key lookup and publishing appends only the new rows.
--
-- kind: 0 commit, 1 tree, 2 blob, 3 chunk.
CREATE TABLE IF NOT EXISTS serve_published_objects (
    kind INTEGER NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY (kind, hash)
) WITHOUT ROWID;
