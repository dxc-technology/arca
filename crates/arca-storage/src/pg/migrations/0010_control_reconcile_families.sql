-- HA hardening R5 (TD-016) — last-write timestamps for the control-plane
-- families that previously replicated in real time only.
--
-- user_grants / team_grants / team_members / bucket_tags join the
-- control-snapshot LWW reconcile, which needs a per-row last-write timestamp
-- (bucket_config and server_config already carry one). The join tables have no
-- created_at to backfill from, so existing rows backfill to NOW(): a
-- pre-upgrade attach loses only against changes made after the upgrade, and no
-- tombstones exist yet for these families that it could spuriously beat.
-- Every writer sets updated_at explicitly; the DEFAULT only covers the
-- backfill of pre-existing rows.
ALTER TABLE user_grants ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE team_grants ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE team_members ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE bucket_tags ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
