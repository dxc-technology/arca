-- Phase 29 HA — node-local monotonic write counter for the objects table.
--
-- `seq` is the cluster anti-entropy changed-since cursor: a peer tracks the
-- highest seq it has applied from this node and asks for everything newer. It
-- is a per-node counter (NOT a replicated field of ObjectRecord): every local
-- write, including apply_remote_object, gets a fresh value, so reconciliation
-- propagates transitively A->B->C.
--
-- A dedicated SEQUENCE is concurrency-safe under the connection pool (unlike a
-- MAX(seq)+1 read-modify-write). Adding the column with a volatile nextval
-- default rewrites the table once and assigns every existing row a distinct
-- positive seq, so a from-zero manifest scan (seq > 0) returns them. The
-- default persists, so future INSERTs that omit seq auto-populate it.
CREATE SEQUENCE objects_seq;
ALTER TABLE objects ADD COLUMN seq BIGINT NOT NULL DEFAULT nextval('objects_seq');
CREATE INDEX idx_objects_seq ON objects(seq);
