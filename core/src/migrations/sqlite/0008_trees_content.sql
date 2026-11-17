-- Store each tree as a single zstd-compressed blob (its canonical hash
-- preimage) in trees.content, instead of exploding it into one row per entry in
-- tree_entries. On a large repo, tree_entries + its indexes were ~77% of the
-- database; one compressed blob per tree collapses that dramatically.
--
-- Nullable + lazy: rows written before this migration have content = NULL and
-- are still read from tree_entries (see get_tree's fallback). New writes set
-- content and stop writing tree_entries. The `oak maintenance compact` command
-- backfills content for legacy trees and then drops tree_entries.
ALTER TABLE trees ADD COLUMN content BLOB;
