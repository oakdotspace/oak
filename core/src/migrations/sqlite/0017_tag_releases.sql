-- Tags are also releases (oak.space `release_protocol` v1). A tag synced from
-- the server carries optional release metadata, and a release's assets are
-- listed in `tag_assets` by blob hash only: asset bytes are fetched on demand
-- (`oak release download`), never on pull or clone.
--
-- `synced = 1` marks a row that mirrors the server's snapshot; a pull
-- replaces every synced row. Rows written by the local `create_tag` (git
-- interop, tests) keep `synced = 0` and are left alone.
ALTER TABLE tags ADD COLUMN title TEXT;
ALTER TABLE tags ADD COLUMN notes TEXT;
ALTER TABLE tags ADD COLUMN is_prerelease INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tags ADD COLUMN finalized INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tags ADD COLUMN published_at TEXT;
ALTER TABLE tags ADD COLUMN created_by TEXT NOT NULL DEFAULT '';
ALTER TABLE tags ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tags ADD COLUMN synced INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS tag_assets (
    tag_name     TEXT NOT NULL,
    name         TEXT NOT NULL,
    blob_hash    TEXT NOT NULL,
    size         INTEGER NOT NULL,
    content_type TEXT NOT NULL,
    PRIMARY KEY (tag_name, name)
);
