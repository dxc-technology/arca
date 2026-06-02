-- Phase 29 HA — last-write timestamp for control-plane LWW reconcile.
--
-- updated_at is the last-writer-wins timestamp used by the periodic
-- full-snapshot control-plane merge. It is NOT carried in the in-memory
-- Credential/User/Team structs: it is a DB column maintained on every write
-- (put_*/update_*/apply_remote_*) and shipped only in the control-snapshot wire
-- entry. Existing rows backfill to created_at (grants/bucket_config/server_config
-- already carry updated_at).
ALTER TABLE credentials ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
UPDATE credentials SET updated_at = created_at;
ALTER TABLE users ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
UPDATE users SET updated_at = created_at;
ALTER TABLE teams ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
UPDATE teams SET updated_at = created_at;
