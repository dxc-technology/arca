-- Phase 29 HA — tombstone flag for hard-deleted object versions.
--
-- In clustered deployments a hard delete marks the row is_tombstone = TRUE
-- (blob cleared) instead of removing it, so the deletion propagates via the
-- changed-since manifest and is not resurrected by anti-entropy. Tombstones are
-- invisible to all S3 reads (recompute_is_latest excludes them, so any
-- is_latest = TRUE query skips them) and GC'd after a grace period. Single-node
-- deployments never set it (hard deletes remove the row outright).
ALTER TABLE objects ADD COLUMN is_tombstone BOOLEAN NOT NULL DEFAULT FALSE;
