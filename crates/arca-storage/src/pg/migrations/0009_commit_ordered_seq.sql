-- Phase 29.1 HA hardening (review §2.2) — commit-ordered seq cursor.
--
-- The v5 SEQUENCE is not transactional: a transaction holding seq=N can still
-- be in flight while seq=N+1 commits. A peer pulling the changed-since
-- manifest then sees N+1, advances its high-water mark past N, and the row
-- committed later with seq=N is never delivered by the incremental sync —
-- silent replication loss under concurrent writes.
--
-- The fix mirrors the SQLite `object_seq` counter: a single-row table updated
-- with `UPDATE ... RETURNING` inside the same transaction as the row. The row
-- lock serializes the assignment until commit, so seq order = commit order: by
-- the time any reader observes seq=N+1 committed, seq=N is either committed
-- and visible or aborted forever (a harmless gap). The cost is serializing the
-- final stretch of concurrent object writes on the counter row — accepted, and
-- no worse than the SQLite backend, whose single write connection serializes
-- every write entirely (decision H3 of the hardening plan).
--
-- Seeded with GREATEST(MAX(seq), sequence last_value): MAX(seq) alone could
-- re-issue values consumed by the sequence for since-deleted rows, and a
-- peer's cursor may already sit above MAX(seq).
CREATE TABLE object_seq (value BIGINT NOT NULL);
INSERT INTO object_seq (value)
SELECT GREATEST(
    COALESCE((SELECT MAX(seq) FROM objects), 0),
    (SELECT last_value FROM objects_seq)
);
ALTER TABLE objects ALTER COLUMN seq DROP DEFAULT;
DROP SEQUENCE objects_seq;
