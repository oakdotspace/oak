-- Squashed baseline schema (migrations 001..011 collapsed into one file).
--
-- Generated from `sqlite3 .schema` against an in-memory database that had
-- every historical migration applied in order. Replaces the per-feature
-- migration chain that accumulated as the local-side schema evolved
-- (organization add/remove, legacy manifests → trees, etc).
--
-- DO NOT EDIT BY HAND. Future schema changes go in new migration files
-- (0001_*, 0002_*, ...). To regenerate, see CLAUDE.md: open a fresh DB,
-- apply every migration in order, then dump the result.
--
-- Idempotency: the migration runner consults a per-version characteristic
-- object (see `migration_already_applied` in sqlite.rs) and skips this
-- baseline if `commits` already exists, so existing repositories never
-- re-run this DDL.

CREATE TABLE blobs (
    hash TEXT PRIMARY KEY,
    content BLOB NOT NULL,
    size INTEGER NOT NULL
);

CREATE TABLE chunks (
    hash TEXT PRIMARY KEY,
    content BLOB NOT NULL,
    size INTEGER NOT NULL
);

CREATE TABLE blob_chunks (
    blob_hash TEXT NOT NULL,
    chunk_hash TEXT NOT NULL REFERENCES chunks(hash),
    chunk_index INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (blob_hash, chunk_index)
);

CREATE TABLE trees (
    hash TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE tree_entries (
    tree_hash TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('tree','blob')),
    hash TEXT NOT NULL,
    mode TEXT NOT NULL DEFAULT 'regular',
    PRIMARY KEY (tree_hash, name),
    FOREIGN KEY (tree_hash) REFERENCES trees(hash)
);
CREATE INDEX idx_tree_entries_tree ON tree_entries(tree_hash);

CREATE TABLE branches (
    name TEXT PRIMARY KEY,
    description TEXT,
    -- parent_branch is a soft pointer, not a foreign key. Locally, `main`
    -- doesn't exist as a branch row (it lives only on the server), but
    -- every personal/feature branch records `parent_branch = "main"` so
    -- the squash-merge target is unambiguous after push. A FK here would
    -- reject those inserts.
    parent_branch TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE commits (
    hash TEXT PRIMARY KEY,
    -- branch_name is a soft label, not a foreign key. Locally, `main`
    -- doesn't exist as a branch row but commits on `main` can flow in
    -- through pull / on-demand fetch. A FK here would reject those.
    branch_name TEXT NOT NULL,
    parent_hash TEXT,
    manifest_hash TEXT NOT NULL,
    author TEXT NOT NULL,
    -- Commit messages no longer exist for local commits or feature-branch
    -- commits on the server. The only commits that carry a message are
    -- squash-merge commits onto `main`, where the message is the merged
    -- branch's description.
    message TEXT,
    timestamp TEXT NOT NULL,
    merge_parent_hash TEXT,
    FOREIGN KEY (parent_hash) REFERENCES commits(hash)
);
CREATE INDEX idx_commits_branch ON commits(branch_name);
CREATE INDEX idx_commits_parent ON commits(parent_hash);
CREATE INDEX idx_commits_timestamp ON commits(timestamp);

CREATE TABLE commit_files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    commit_hash TEXT NOT NULL,
    path TEXT NOT NULL,
    change_type TEXT NOT NULL,
    old_blob_hash TEXT,
    new_blob_hash TEXT,
    old_path TEXT,
    FOREIGN KEY (commit_hash) REFERENCES commits(hash)
);
CREATE INDEX idx_commit_files_commit ON commit_files(commit_hash);

CREATE TABLE branch_heads (
    -- branch_name is a soft label here too — same reason as commits.branch_name.
    branch_name TEXT PRIMARY KEY,
    head_hash TEXT NOT NULL,
    FOREIGN KEY (head_hash) REFERENCES commits(hash)
);

CREATE TABLE tags (
    name TEXT PRIMARY KEY,
    commit_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (commit_hash) REFERENCES commits(hash)
);

CREATE TABLE metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
