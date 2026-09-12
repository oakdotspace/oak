CREATE INDEX IF NOT EXISTS idx_commits_merge_parent
ON commits(merge_parent_hash)
WHERE merge_parent_hash IS NOT NULL;
