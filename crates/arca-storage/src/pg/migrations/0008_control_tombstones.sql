-- Phase 29 HA — control-plane deletion tombstones.
--
-- A hard delete of a control-plane entity (credential, user, team, grant,
-- bucket) leaves a tombstone here so the deletion propagates via the
-- control-snapshot reconcile and is not resurrected by a peer that still holds
-- the live row (the resurrection trap the object tombstones solve for the data
-- plane). Reconcile-only: reads of the entities are unaffected. GC'd after a
-- grace window. Single-node deployments never write here.
CREATE TABLE control_tombstones (
    entity_type TEXT NOT NULL,
    entity_key  TEXT NOT NULL,
    deleted_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (entity_type, entity_key)
);
