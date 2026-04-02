-- Initial PostgreSQL schema for Arca.
-- Equivalent to SQLite migrations v1-v13 consolidated into a single migration.

-- Credentials (v1 + v5 admin + v8 user_id)
CREATE TABLE credentials (
    access_key_id     TEXT PRIMARY KEY NOT NULL,
    secret_access_key TEXT NOT NULL,
    description       TEXT NOT NULL DEFAULT '',
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    active            BOOLEAN NOT NULL DEFAULT TRUE,
    admin             BOOLEAN NOT NULL DEFAULT FALSE,
    user_id           TEXT NOT NULL DEFAULT 'root'
);

-- Buckets (v2 + v8 owner)
CREATE TABLE buckets (
    name       TEXT PRIMARY KEY NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    owner      TEXT NOT NULL DEFAULT 'root'
);

-- Objects (v3 + v6 metadata + v7 encryption + v9 versioning + v12 Object Lock + v13 checksums)
CREATE TABLE objects (
    bucket               TEXT NOT NULL,
    key                  TEXT NOT NULL,
    version_id           TEXT,
    blob_id              TEXT NOT NULL DEFAULT '',
    size                 BIGINT NOT NULL DEFAULT 0,
    etag                 TEXT NOT NULL DEFAULT '',
    content_type         TEXT,
    last_modified        TIMESTAMPTZ NOT NULL,
    metadata             JSONB NOT NULL DEFAULT '{}',
    encryption_algorithm TEXT,
    encryption_key_id    TEXT,
    owner                TEXT NOT NULL DEFAULT 'root',
    is_latest            BOOLEAN NOT NULL DEFAULT TRUE,
    is_delete_marker     BOOLEAN NOT NULL DEFAULT FALSE,
    retention_mode       TEXT,
    retain_until_date    TIMESTAMPTZ,
    legal_hold_status    TEXT,
    storage_class        TEXT NOT NULL DEFAULT 'STANDARD',
    checksum_algorithm   TEXT,
    checksum_value       TEXT
);

-- Enforce exactly one is_latest=true row per (bucket, key).
CREATE UNIQUE INDEX idx_objects_latest
    ON objects(bucket, key) WHERE is_latest = TRUE;

-- Fast version listing by key.
CREATE INDEX idx_objects_versions
    ON objects(bucket, key, last_modified DESC);

-- Multipart uploads (v4 + v6 metadata + v13 checksum_algorithm)
CREATE TABLE multipart_uploads (
    upload_id          TEXT PRIMARY KEY NOT NULL,
    bucket             TEXT NOT NULL,
    key                TEXT NOT NULL,
    content_type       TEXT,
    initiated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata           JSONB NOT NULL DEFAULT '{}',
    checksum_algorithm TEXT
);

-- Parts (v4 + v13 checksum_value + last_modified)
CREATE TABLE parts (
    upload_id      TEXT NOT NULL,
    part_number    INTEGER NOT NULL,
    blob_id        TEXT NOT NULL,
    size           BIGINT NOT NULL,
    etag           TEXT NOT NULL,
    checksum_value TEXT,
    last_modified  TIMESTAMPTZ,
    PRIMARY KEY (upload_id, part_number)
);

-- Bucket config (v7)
CREATE TABLE bucket_config (
    bucket       TEXT NOT NULL,
    config_key   TEXT NOT NULL,
    config_value TEXT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (bucket, config_key)
);

-- RBAC: Users (v8)
CREATE TABLE users (
    user_id     TEXT PRIMARY KEY NOT NULL,
    username    TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    is_root     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- RBAC: Teams (v8)
CREATE TABLE teams (
    team_id     TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE team_members (
    team_id TEXT NOT NULL REFERENCES teams(team_id),
    user_id TEXT NOT NULL REFERENCES users(user_id),
    PRIMARY KEY (team_id, user_id)
);

-- RBAC: Grants (v8)
CREATE TABLE grants (
    grant_id    TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    document    JSONB NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE user_grants (
    user_id  TEXT NOT NULL REFERENCES users(user_id),
    grant_id TEXT NOT NULL REFERENCES grants(grant_id),
    PRIMARY KEY (user_id, grant_id)
);

CREATE TABLE team_grants (
    team_id  TEXT NOT NULL REFERENCES teams(team_id),
    grant_id TEXT NOT NULL REFERENCES grants(grant_id),
    PRIMARY KEY (team_id, grant_id)
);

-- Root user
INSERT INTO users (user_id, username, description, is_root)
VALUES ('root', 'root', 'System root user', TRUE);

-- Built-in grants
INSERT INTO grants (grant_id, name, description, document)
VALUES ('grant-administrator-access', 'AdministratorAccess',
    'Full access to all operations',
    '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["*"],"Resource":["*"]}]}');

INSERT INTO grants (grant_id, name, description, document)
VALUES ('grant-s3-full-access', 'S3FullAccess',
    'Full access to S3 operations',
    '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:*"],"Resource":["*"]}]}');

INSERT INTO grants (grant_id, name, description, document)
VALUES ('grant-s3-read-only', 'S3ReadOnlyAccess',
    'Read-only access to S3 operations',
    '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:GetObject","s3:ListBucket","s3:ListAllMyBuckets","s3:GetBucketLocation","s3:GetBucketEncryption"],"Resource":["*"]}]}');

-- Attach AdministratorAccess to root user
INSERT INTO user_grants (user_id, grant_id)
VALUES ('root', 'grant-administrator-access');

-- Server config (v10)
CREATE TABLE server_config (
    config_key   TEXT PRIMARY KEY NOT NULL,
    config_value TEXT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Audit log (v10)
CREATE TABLE audit_log (
    id              BIGSERIAL PRIMARY KEY,
    timestamp       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    request_id      TEXT NOT NULL,
    operation       TEXT NOT NULL,
    bucket          TEXT,
    key             TEXT,
    version_id      TEXT,
    user_id         TEXT,
    access_key_id   TEXT,
    source_ip       TEXT,
    http_method     TEXT NOT NULL,
    http_status     INTEGER NOT NULL,
    error_code      TEXT,
    bytes_sent      BIGINT NOT NULL DEFAULT 0,
    bytes_received  BIGINT NOT NULL DEFAULT 0,
    duration_ms     BIGINT NOT NULL DEFAULT 0,
    user_agent      TEXT
);

CREATE INDEX idx_audit_log_timestamp ON audit_log(timestamp);
CREATE INDEX idx_audit_log_bucket ON audit_log(bucket, timestamp);
CREATE INDEX idx_audit_log_user ON audit_log(user_id, timestamp);
CREATE INDEX idx_audit_log_operation ON audit_log(operation, timestamp);

-- Metrics snapshots (v10)
CREATE TABLE metrics_snapshot (
    id                   BIGSERIAL PRIMARY KEY,
    timestamp            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    bucket_count         INTEGER NOT NULL,
    object_count         BIGINT NOT NULL,
    total_size_bytes     BIGINT NOT NULL,
    disk_total_bytes     BIGINT,
    disk_available_bytes BIGINT,
    active_connections   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_metrics_snapshot_timestamp ON metrics_snapshot(timestamp);

-- Object tags (v11)
CREATE TABLE object_tags (
    bucket     TEXT NOT NULL,
    key        TEXT NOT NULL,
    version_id TEXT NOT NULL DEFAULT '',
    tag_key    TEXT NOT NULL,
    tag_value  TEXT NOT NULL,
    PRIMARY KEY (bucket, key, version_id, tag_key)
);

-- Bucket tags (v11)
CREATE TABLE bucket_tags (
    bucket    TEXT NOT NULL,
    tag_key   TEXT NOT NULL,
    tag_value TEXT NOT NULL,
    PRIMARY KEY (bucket, tag_key)
);

-- Notification events (v14)
CREATE TABLE notification_events (
    id                TEXT PRIMARY KEY,
    bucket            TEXT NOT NULL,
    key               TEXT NOT NULL,
    event_name        TEXT NOT NULL,
    event_time        TIMESTAMPTZ NOT NULL,
    payload           TEXT NOT NULL,
    destination_url   TEXT NOT NULL,
    configuration_id  TEXT NOT NULL,
    delivery_status   TEXT NOT NULL DEFAULT 'pending',
    delivery_attempts INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_notification_events_bucket ON notification_events(bucket);
CREATE INDEX idx_notification_events_status ON notification_events(delivery_status);
CREATE INDEX idx_notification_events_created ON notification_events(created_at);
