-- Object receipts owned by an unfinished `oak serve` staged-v1 session.
--
-- Previously every session's tree/blob/chunk sets lived inside the one
-- `staged_push_v1_sessions` metadata JSON value, which each envelope parsed,
-- cloned, compared and rewrote in full — quadratic in the size of a first
-- publication. The session header (branch boundary, totals, commits) stays
-- in that JSON value; the large sets live here.
--
-- kind: 1 tree, 2 blob (mapping = canonical chunk-mapping digest), 3 chunk.
CREATE TABLE IF NOT EXISTS serve_stage_objects (
    stage_id TEXT NOT NULL,
    kind INTEGER NOT NULL,
    hash TEXT NOT NULL,
    mapping TEXT,
    PRIMARY KEY (stage_id, kind, hash)
) WITHOUT ROWID;
