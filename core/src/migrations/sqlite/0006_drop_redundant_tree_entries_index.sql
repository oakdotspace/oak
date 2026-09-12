-- Drop the standalone index on tree_entries(tree_hash). It is fully redundant:
-- the PRIMARY KEY (tree_hash, name) already indexes tree_hash as its leftmost
-- column, so every `WHERE tree_hash = ?` lookup is already covered. On a large
-- repo this duplicate index was ~18% of the database on disk. (The server side
-- dropped its equivalent in postgres migration 0061.)
--
-- Space is returned to the freelist immediately but the file only shrinks after
-- a VACUUM (run by the `oak maintenance compact` command).
DROP INDEX IF EXISTS idx_tree_entries_tree;
