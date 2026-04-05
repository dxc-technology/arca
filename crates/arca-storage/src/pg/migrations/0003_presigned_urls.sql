-- Presigned URL tracking table.
-- Stores metadata about generated presigned URLs (NOT the URL itself).
CREATE TABLE presigned_urls (
    id               TEXT PRIMARY KEY,
    bucket           TEXT NOT NULL,
    key              TEXT NOT NULL,
    method           TEXT NOT NULL DEFAULT 'GET',
    expires_seconds  INTEGER NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at       TIMESTAMPTZ NOT NULL,
    access_key_id    TEXT NOT NULL
);
CREATE INDEX idx_presigned_urls_bucket ON presigned_urls(bucket);
CREATE INDEX idx_presigned_urls_expires ON presigned_urls(expires_at);
CREATE INDEX idx_presigned_urls_bucket_key ON presigned_urls(bucket, key);
