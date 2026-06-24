-- Phase 30: console/CLI-driven long-running maintenance operations
-- (re-encryption, metadata-backend migration, topology migration) are tracked
-- here so progress survives restarts and is observable. One job runs at a time;
-- the worker commits progress per item. Mirrors the SQLite v24 schema (params
-- kept as TEXT JSON so migrate-db can copy the columns verbatim).
CREATE TABLE maintenance_jobs (
    id           TEXT PRIMARY KEY NOT NULL,
    type         TEXT NOT NULL,
    status       TEXT NOT NULL,
    mode         TEXT NOT NULL,
    params       TEXT NOT NULL DEFAULT '{}',
    total        BIGINT NOT NULL DEFAULT 0,
    done         BIGINT NOT NULL DEFAULT 0,
    rate         DOUBLE PRECISION NOT NULL DEFAULT 0,
    last_error   TEXT,
    created_at   TIMESTAMPTZ NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL,
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ
);
CREATE INDEX idx_maintenance_jobs_status ON maintenance_jobs(status);
CREATE INDEX idx_maintenance_jobs_created ON maintenance_jobs(created_at DESC);

CREATE TABLE maintenance_job_logs (
    seq      BIGSERIAL PRIMARY KEY,
    job_id   TEXT NOT NULL,
    ts       TIMESTAMPTZ NOT NULL,
    level    TEXT NOT NULL,
    message  TEXT NOT NULL
);
CREATE INDEX idx_maintenance_job_logs_job ON maintenance_job_logs(job_id, seq DESC);
