//! The serialization gate of the objects-writing PostgreSQL transactions.
//!
//! Under READ COMMITTED a plain SELECT never blocks, so the authoritative CAS
//! read of a write must come after a statement that waits until every earlier
//! writer of the same key has committed: the gate, the FIRST statement of
//! every objects-writing transaction. It also hands out the row's node-local
//! `seq`.
//!
//! - Cluster mode: the single-row `object_seq` counter (`UPDATE ... RETURNING`,
//!   [`next_object_seq`]). Its row lock serializes every object write until
//!   commit, so seq order = commit order, which the anti-entropy
//!   changed-since cursor needs (decision H3).
//! - Single node: nothing reads seq in commit order, and the CAS read only
//!   needs the writers of the SAME key serialized. The gate is a
//!   transaction-level advisory lock on a hash of `(bucket, key)`
//!   ([`key_lock_id`]) and seq comes from the non-transactional
//!   `object_seq_local` sequence (migration 0018), so writes to different keys
//!   run in parallel. A hash collision only adds a wait, never a wrong result
//!   (decision H13).
//!
//! LOCK-ORDER RULE: the gate comes before the first row-mutating statement of
//! every objects-writing transaction; a batch takes all its key locks first,
//! in ascending id order ([`lock_keys`]), so two overlapping batches cannot
//! deadlock. Paths that take no gate (`purge_tombstones`, the replication
//! status update) only lock rows and never wait for a gate while holding one,
//! so they cannot close a cycle.
//!
//! Each run uses one generator, and [`reconcile_object_seq`] raises both to
//! the highest seq in use when the store opens, so a switch between modes (by
//! `migrate-topology` or by editing the config) never issues a seq below, or
//! equal to, one already stored.

use arca_core::error::ArcaError;
use sqlx_core::row::Row;

type PgTx<'c> = sqlx_core::transaction::Transaction<'c, sqlx_postgres::Postgres>;

/// The 64-bit FNV-1a hash. A fixed algorithm, not `DefaultHasher`, whose
/// output Rust does not guarantee across releases: every process that writes
/// to the database (the server, the offline tools) must take the same lock for
/// the same key.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The advisory lock id of a key: the hash of `bucket NUL key`. A bucket name
/// never contains NUL, so `("ab", "c")` and `("a", "bc")` hash different input.
pub(super) fn key_lock_id(bucket: &str, key: &str) -> i64 {
    let mut input = Vec::with_capacity(bucket.len() + 1 + key.len());
    input.extend_from_slice(bucket.as_bytes());
    input.push(0);
    input.extend_from_slice(key.as_bytes());
    fnv1a64(&input) as i64
}

/// The lock ids of a batch's keys, ascending and without duplicates: the
/// acquisition order of [`lock_keys`].
pub(super) fn sorted_key_lock_ids<'k>(bucket: &str, keys: impl Iterator<Item = &'k str>) -> Vec<i64> {
    let mut ids: Vec<i64> = keys.map(|key| key_lock_id(bucket, key)).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Opens an objects-writing transaction on one key: takes the gate of the
/// mode and returns the seq to stamp. Must be the transaction's first
/// statement (see the module documentation).
pub(super) async fn begin_object_write(
    tx: &mut PgTx<'_>,
    cluster_mode: bool,
    bucket: &str,
    key: &str,
) -> Result<i64, sqlx_core::error::Error> {
    if cluster_mode {
        return next_object_seq(tx).await;
    }
    // One round trip: the FROM item takes the lock, the row carries the seq.
    let row = sqlx_core::query::query(
        "SELECT nextval('object_seq_local') AS seq FROM (SELECT pg_advisory_xact_lock($1)) AS gate",
    )
    .bind(key_lock_id(bucket, key))
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.get::<i64, _>("seq"))
}

/// Takes the single-node gate of every key of a batch, in the order of `ids`
/// (from [`sorted_key_lock_ids`]). Each entry then gets its seq from
/// [`next_seq`]. PostgreSQL evaluates a volatile output expression after the
/// sort, so the locks are taken in `ord` order.
pub(super) async fn lock_keys(tx: &mut PgTx<'_>, ids: &[i64]) -> Result<(), sqlx_core::error::Error> {
    sqlx_core::query::query(
        "SELECT pg_advisory_xact_lock(id) FROM unnest($1::bigint[]) WITH ORDINALITY AS t(id, ord) \
         ORDER BY ord",
    )
    .bind(ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The next seq of the mode, without a gate: for a transaction that already
/// holds its keys' locks ([`lock_keys`]), or in cluster mode, where taking the
/// counter is the gate.
pub(super) async fn next_seq(tx: &mut PgTx<'_>, cluster_mode: bool) -> Result<i64, sqlx_core::error::Error> {
    if cluster_mode {
        return next_object_seq(tx).await;
    }
    let row = sqlx_core::query::query("SELECT nextval('object_seq_local') AS seq")
        .fetch_one(&mut **tx)
        .await?;
    Ok(row.get::<i64, _>("seq"))
}

/// Returns the next commit-ordered seq, advancing the single-row `object_seq`
/// counter INSIDE the caller's transaction (migration 0009): the cluster-mode
/// gate. The `UPDATE ... RETURNING` takes the counter's row lock until commit,
/// which serializes the assignment, so a peer's changed-since manifest cursor
/// can never skip a row still in flight (decision H3: the SEQUENCE of v5 was
/// not transactional and lost rows). Mirrors the SQLite `next_object_seq`.
pub(super) async fn next_object_seq(tx: &mut PgTx<'_>) -> Result<i64, sqlx_core::error::Error> {
    let row = sqlx_core::query::query("UPDATE object_seq SET value = value + 1 RETURNING value")
        .fetch_one(&mut **tx)
        .await?;
    Ok(row.get::<i64, _>("value"))
}

/// Raises the `object_seq` counter and the `object_seq_local` sequence to the
/// highest seq in use (the counter, the sequence, every row), never lowering
/// either, and returns the counter. Run when the store opens, before it
/// serves, and by `migrate-topology --to-cluster`.
///
/// The counter is raised under its row lock, atomically with respect to any
/// cluster-mode writer. The sequence is set only when it lags, which happens
/// after a run in the other mode or a copy by `migrate-db`; a single-node
/// server that is already running has raised it at its own start and only
/// moves it up, so a tool opening the database next to it leaves it alone.
pub(super) async fn reconcile_object_seq(pool: &sqlx_postgres::PgPool) -> Result<i64, ArcaError> {
    let err = |e: sqlx_core::error::Error| ArcaError::Internal(format!("reconcile_object_seq: {e}"));
    let mut tx = pool.begin().await.map_err(err)?;
    let row = sqlx_core::query::query(
        "UPDATE object_seq SET value = GREATEST(value, \
             (SELECT COALESCE(MAX(seq), 0) FROM objects), \
             (SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM object_seq_local)) \
         RETURNING value",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(err)?;
    let highest = row.get::<i64, _>("value");
    sqlx_core::query::query(
        "SELECT setval('object_seq_local', $1) FROM object_seq_local \
         WHERE NOT is_called OR last_value < $1",
    )
    .bind(highest)
    .execute(&mut *tx)
    .await
    .map_err(err)?;
    tx.commit().await.map_err(err)?;
    Ok(highest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_the_reference_vectors() {
        // Reference values of the FNV-1a 64-bit hash (offset basis, "a", "foobar").
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn key_lock_id_hashes_bucket_nul_key() {
        assert_eq!(key_lock_id("b", "k"), fnv1a64(b"b\0k") as i64);
    }

    #[test]
    fn key_lock_id_keeps_the_bucket_key_boundary() {
        assert_ne!(key_lock_id("ab", "c"), key_lock_id("a", "bc"));
    }

    #[test]
    fn key_lock_ids_are_sorted_and_deduplicated() {
        let ids = sorted_key_lock_ids("b", ["x", "y", "x", "z"].into_iter());
        assert_eq!(ids.len(), 3);
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        for key in ["x", "y", "z"] {
            assert!(ids.contains(&key_lock_id("b", key)));
        }
    }
}
