-- Durable local lookup for canonical working-tree captures.
--
-- Portable change identity is stored once. Capture occurrences remain
-- append-only and keep provenance separate so identical changes captured on
-- different branches never overwrite one another.

CREATE TABLE IF NOT EXISTS change_sets (
    change_id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    object_format TEXT NOT NULL,
    base_tree_hash TEXT NOT NULL,
    result_tree_hash TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_paths_json TEXT NOT NULL,
    descriptor_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS change_captures (
    capture_id TEXT PRIMARY KEY,
    change_id TEXT NOT NULL REFERENCES change_sets(change_id),
    format_version INTEGER NOT NULL,
    repo_owner TEXT NOT NULL,
    repo_name TEXT NOT NULL,
    origin TEXT,
    source_branch TEXT NOT NULL,
    base_commit_hash TEXT,
    captured_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_change_captures_change_id
    ON change_captures(change_id);
