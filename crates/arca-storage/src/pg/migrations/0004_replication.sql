-- Replication journal and replication_status on objects.
-- Mirrors SQLite migration v17.

ALTER TABLE objects ADD COLUMN replication_status TEXT;

CREATE TABLE replication_journal (
    id                   TEXT PRIMARY KEY,
    bucket               TEXT NOT NULL,
    key                  TEXT NOT NULL,
    version_id           TEXT,
    rule_id              TEXT NOT NULL,
    event_type           TEXT NOT NULL,
    destination_endpoint TEXT NOT NULL,
    destination_bucket   TEXT NOT NULL,
    status               TEXT NOT NULL DEFAULT 'pending',
    attempts             INTEGER NOT NULL DEFAULT 0,
    last_error           TEXT,
    next_retry_at        TIMESTAMPTZ NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_replication_journal_claim
    ON replication_journal(status, next_retry_at);
CREATE INDEX idx_replication_journal_bucket
    ON replication_journal(bucket, created_at);
CREATE INDEX idx_replication_journal_updated
    ON replication_journal(updated_at);
