CREATE TABLE IF NOT EXISTS serve_mapping_proof_jobs (
    token TEXT PRIMARY KEY,
    request_digest TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL CHECK (status IN ('uploading', 'pending', 'running', 'complete', 'conflict')),
    terminal_code TEXT,
    worker_token TEXT,
    lease_expires_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((status = 'conflict' AND terminal_code IS NOT NULL)
        OR (status <> 'conflict' AND terminal_code IS NULL))
);

CREATE TABLE IF NOT EXISTS serve_mapping_proof_blobs (
    token TEXT NOT NULL REFERENCES serve_mapping_proof_jobs(token) ON DELETE CASCADE,
    blob_index INTEGER NOT NULL,
    hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    mapping_digest TEXT NOT NULL,
    total_chunks INTEGER NOT NULL,
    base_mapping_digest TEXT,
    verified INTEGER NOT NULL DEFAULT 0,
    missing INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (token, blob_index),
    UNIQUE (token, hash)
);

CREATE TABLE IF NOT EXISTS serve_mapping_proof_pages (
    token TEXT NOT NULL,
    blob_index INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL,
    hash TEXT NOT NULL,
    offset INTEGER NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (token, blob_index, chunk_index),
    FOREIGN KEY (token, blob_index)
        REFERENCES serve_mapping_proof_blobs(token, blob_index) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_serve_mapping_proof_jobs_expiry
    ON serve_mapping_proof_jobs(status, updated_at);
