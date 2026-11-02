-- Relax the strict FK constraints on `branches`, `branch_heads`, and
-- `commits`, and make `commits.message` nullable.
--
-- Why: the new branch model treats `parent_branch` and `branch_name` as
-- soft labels. Locally, `main` doesn't exist as a branches row but is the
-- legitimate `parent_branch` for every personal branch, and commits with
-- `branch_name = "main"` can flow in via pull. The FKs that referenced
-- `branches(name)` rejected those legitimate inserts. Likewise, local
-- commits and feature-branch commits no longer carry messages — message
-- is `None` and the column must accept NULL. The current baseline schema
-- already reflects this, but databases created under the older squashed
-- baseline still have the strict constraints and need this fix-up.
--
-- SQLite can't ALTER a column to drop a NOT NULL or FK constraint, so we
-- rebuild each affected table. Two PRAGMAs gate the rebuild:
--
--   foreign_keys=OFF: stops DROP from firing ON-DELETE actions for the
--     (about-to-be-dangling) child FKs.
--   legacy_alter_table=ON: stops ALTER TABLE RENAME from rewriting
--     inline FK references in *other* tables. Without this, renaming the
--     old `commits` table to `_commits_old` rewrites the (already-new)
--     `branch_heads.head_hash` FK to reference `_commits_old`, which is
--     then dropped — leaving a dangling reference in the final schema.
--
-- We use the "new_X / drop X / rename new_X -> X" pattern from
-- https://sqlite.org/lang_altertable.html#otheralter, which avoids
-- renaming the original tables at all and so doesn't depend on the
-- legacy_alter_table semantics. The PRAGMA is set defensively anyway.

PRAGMA foreign_keys = OFF;
PRAGMA legacy_alter_table = ON;

BEGIN;

-- commits: drop FK on branch_name -> branches(name); keep parent_hash FK;
-- make message nullable.
CREATE TABLE new_commits (
    hash TEXT PRIMARY KEY,
    branch_name TEXT NOT NULL,
    parent_hash TEXT,
    manifest_hash TEXT NOT NULL,
    author TEXT NOT NULL,
    message TEXT,
    timestamp TEXT NOT NULL,
    merge_parent_hash TEXT,
    FOREIGN KEY (parent_hash) REFERENCES commits(hash)
);
INSERT INTO new_commits (hash, branch_name, parent_hash, manifest_hash, author, message, timestamp, merge_parent_hash)
    SELECT hash, branch_name, parent_hash, manifest_hash, author, message, timestamp, merge_parent_hash FROM commits;
DROP TABLE commits;
ALTER TABLE new_commits RENAME TO commits;
CREATE INDEX idx_commits_branch ON commits(branch_name);
CREATE INDEX idx_commits_parent ON commits(parent_hash);
CREATE INDEX idx_commits_timestamp ON commits(timestamp);

-- branch_heads: drop FK on branch_name -> branches(name); keep head_hash FK.
CREATE TABLE new_branch_heads (
    branch_name TEXT PRIMARY KEY,
    head_hash TEXT NOT NULL,
    FOREIGN KEY (head_hash) REFERENCES commits(hash)
);
INSERT INTO new_branch_heads (branch_name, head_hash)
    SELECT branch_name, head_hash FROM branch_heads;
DROP TABLE branch_heads;
ALTER TABLE new_branch_heads RENAME TO branch_heads;

-- branches: drop FK on parent_branch -> branches(name).
CREATE TABLE new_branches (
    name TEXT PRIMARY KEY,
    description TEXT,
    parent_branch TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO new_branches (name, description, parent_branch, status, created_at)
    SELECT name, description, parent_branch, status, created_at FROM branches;
DROP TABLE branches;
ALTER TABLE new_branches RENAME TO branches;

COMMIT;

PRAGMA legacy_alter_table = OFF;
PRAGMA foreign_keys = ON;
