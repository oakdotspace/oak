-- Drop the FK on `commits.parent_hash`, making parent_hash a soft pointer
-- like `branch_name` already is.
--
-- Why: after a server-side squash merge, the CLI auto-syncs `main`'s new
-- HEAD into local SQLite by synthesizing a single commit row that carries
-- the server's real `parent_hash` (so the LCA finder behaves on the next
-- pull). On a fresh-ish clone the local DB has never seen main's prior
-- squash commit, so the FK `parent_hash REFERENCES commits(hash)` rejects
-- the insert with "FOREIGN KEY constraint failed" and auto-sync gives up.
-- The user sees the merge succeed on the server but ends up with their
-- new personal branch seeded at the old branch tip instead of main's new
-- head, and the same FK prevents `oak pull` from recovering. Walking the
-- full ancestor chain on every fetch would work, but is more wire traffic
-- than we need — treating `parent_hash` as a soft pointer matches what
-- `branch_name` already does in this same table.
--
-- SQLite can't ALTER a column to drop a FK, so we rebuild the table.
-- Same PRAGMA gating and "new_X / drop X / rename new_X -> X" pattern as
-- 0001_relax_branch_fks_and_message.

PRAGMA foreign_keys = OFF;
PRAGMA legacy_alter_table = ON;

BEGIN;

CREATE TABLE new_commits (
    hash TEXT PRIMARY KEY,
    branch_name TEXT NOT NULL,
    parent_hash TEXT,
    manifest_hash TEXT NOT NULL,
    author TEXT NOT NULL,
    message TEXT,
    timestamp TEXT NOT NULL,
    merge_parent_hash TEXT
);
INSERT INTO new_commits (hash, branch_name, parent_hash, manifest_hash, author, message, timestamp, merge_parent_hash)
    SELECT hash, branch_name, parent_hash, manifest_hash, author, message, timestamp, merge_parent_hash FROM commits;
DROP TABLE commits;
ALTER TABLE new_commits RENAME TO commits;
CREATE INDEX idx_commits_branch ON commits(branch_name);
CREATE INDEX idx_commits_parent ON commits(parent_hash);
CREATE INDEX idx_commits_timestamp ON commits(timestamp);

COMMIT;

PRAGMA legacy_alter_table = OFF;
PRAGMA foreign_keys = ON;
