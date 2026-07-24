//! PostgreSQL read/write primitives for the generic backend migration.
//!
//! Reads use `try_get::<Option<T>>` typed by the column's [`ColumnKind`]
//! (TIMESTAMPTZ → `DateTime<Utc>`, BOOLEAN → `bool`, JSONB → `serde_json::Value`,
//! BIGINT → `i64`, INTEGER → `i32`, DOUBLE PRECISION → `f64`, else TEXT →
//! `String`), so NULLs round-trip. Writes bind each [`Cell`] to its PG type.
//! Multi-row INSERTs are built with `$1,$2,…` placeholders and bound cell by
//! cell, batched to keep the parameter count bounded.

use chrono::{DateTime, Utc};
use sqlx_core::query::Query;
use sqlx_core::row::Row;
use sqlx_postgres::{PgArguments, Postgres};

use super::{Cell, ColumnKind, TableDesc, BATCH_SIZE};
use crate::pg::PgStore;

/// PG has a 65535 bind-parameter ceiling per statement. With ~24 columns the
/// 500-row batch stays well under it, but clamp defensively anyway.
const MAX_PARAMS: usize = 60000;

/// Reads one column of a PG row into a [`Cell`] per its logical kind.
fn read_cell(
    row: &sqlx_postgres::PgRow,
    idx: usize,
    kind: ColumnKind,
) -> Result<Cell, String> {
    let map = |e: sqlx_core::error::Error| format!("column {idx}: {e}");
    match kind {
        ColumnKind::Int => Ok(row
            .try_get::<Option<i64>, _>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Int)),
        ColumnKind::SmallInt => Ok(row
            .try_get::<Option<i32>, _>(idx)
            .map_err(map)?
            .map_or(Cell::Null, |v| Cell::Int(v as i64))),
        ColumnKind::Float => Ok(row
            .try_get::<Option<f64>, _>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Float)),
        ColumnKind::Bool => Ok(row
            .try_get::<Option<bool>, _>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Bool)),
        ColumnKind::Timestamp => Ok(row
            .try_get::<Option<DateTime<Utc>>, _>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Ts)),
        ColumnKind::Json => Ok(row
            .try_get::<Option<serde_json::Value>, _>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Json)),
        ColumnKind::Text => Ok(row
            .try_get::<Option<String>, _>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Text)),
    }
}

/// Binds one [`Cell`] to a PG query argument, typed per the column kind so the
/// right PG OID is used even for NULLs. Owned values are bound (cloned where
/// needed) to keep the query `'static` in its bound data.
fn bind_cell<'q>(
    q: Query<'q, Postgres, PgArguments>,
    cell: &Cell,
    kind: ColumnKind,
) -> Query<'q, Postgres, PgArguments> {
    match cell {
        Cell::Null => match kind {
            ColumnKind::Int => q.bind(None::<i64>),
            ColumnKind::SmallInt => q.bind(None::<i32>),
            ColumnKind::Float => q.bind(None::<f64>),
            ColumnKind::Bool => q.bind(None::<bool>),
            ColumnKind::Timestamp => q.bind(None::<DateTime<Utc>>),
            ColumnKind::Json => q.bind(None::<serde_json::Value>),
            ColumnKind::Text => q.bind(None::<String>),
        },
        Cell::Int(i) => match kind {
            ColumnKind::SmallInt => q.bind(*i as i32),
            _ => q.bind(*i),
        },
        Cell::Float(f) => q.bind(*f),
        Cell::Bool(b) => q.bind(*b),
        Cell::Ts(ts) => q.bind(*ts),
        Cell::Json(j) => q.bind(j.clone()),
        Cell::Text(s) => q.bind(s.clone()),
    }
}

/// Reads every row of `table` as ordered cells.
pub async fn dump_table(store: &PgStore, table: &TableDesc) -> Result<Vec<Vec<Cell>>, String> {
    let col_list = table
        .columns
        .iter()
        .map(|c| c.name)
        .collect::<Vec<_>>()
        .join(", ");
    // ORDER BY the first column for deterministic output (helps audit/metrics
    // surrogate-id ordering and reproducible tests).
    let first = table.columns[0].name;
    let sql = format!("SELECT {col_list} FROM {} ORDER BY {first}", table.name);

    let rows = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
        .fetch_all(store.pool())
        .await
        .map_err(|e| format!("dump {}: {e}", table.name))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut cells = Vec::with_capacity(table.columns.len());
        for (idx, c) in table.columns.iter().enumerate() {
            cells.push(read_cell(row, idx, c.kind)?);
        }
        out.push(cells);
    }
    Ok(out)
}

/// Inserts `rows` into `table`, batched to stay under the bind-parameter ceiling.
pub async fn load_table(
    store: &PgStore,
    table: &TableDesc,
    rows: &[Vec<Cell>],
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let col_list = table
        .columns
        .iter()
        .map(|c| c.name)
        .collect::<Vec<_>>()
        .join(", ");
    let num_cols = table.columns.len();
    let rows_per_batch = (MAX_PARAMS / num_cols.max(1)).min(BATCH_SIZE).max(1);

    for chunk in rows.chunks(rows_per_batch) {
        // Build "($1,$2,..),($n,..)" with running placeholder numbers.
        let mut placeholders = String::new();
        let mut p = 1usize;
        for r in 0..chunk.len() {
            if r > 0 {
                placeholders.push(',');
            }
            placeholders.push('(');
            for c in 0..num_cols {
                if c > 0 {
                    placeholders.push(',');
                }
                placeholders.push('$');
                placeholders.push_str(&p.to_string());
                p += 1;
            }
            placeholders.push(')');
        }
        let sql = format!("INSERT INTO {} ({col_list}) VALUES {placeholders}", table.name);

        let mut q = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        for row in chunk {
            for (cell, c) in row.iter().zip(table.columns.iter()) {
                q = bind_cell(q, cell, c.kind);
            }
        }
        q.execute(store.pool())
            .await
            .map_err(|e| format!("load {}: {e}", table.name))?;
    }

    // Tables with a BIGSERIAL surrogate primary key were loaded with explicit id
    // values; advance the underlying sequence past them so future server inserts
    // don't collide. (Only audit_log and metrics_snapshot have one.)
    if matches!(table.name, "audit_log" | "metrics_snapshot") {
        let sql = format!(
            "SELECT setval(pg_get_serial_sequence('{}', 'id'), \
             GREATEST((SELECT COALESCE(MAX(id), 0) FROM {}), 1))",
            table.name, table.name
        );
        sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .execute(store.pool())
            .await
            .map_err(|e| format!("reset sequence for {}: {e}", table.name))?;
    }
    Ok(())
}

/// Number of rows in `table`.
pub async fn count(store: &PgStore, table: &str) -> Result<u64, String> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .map_err(|e| format!("count {table}: {e}"))?;
    let n: i64 = row.get(0);
    Ok(n as u64)
}

/// Removes every row from `table`. CASCADE so FK-referenced parents truncate too
/// (the orchestrator truncates in reverse FK order, but CASCADE is belt-and-braces).
pub async fn truncate(store: &PgStore, table: &str) -> Result<(), String> {
    let sql = format!("TRUNCATE TABLE {table} CASCADE");
    sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
        .execute(store.pool())
        .await
        .map_err(|e| format!("truncate {table}: {e}"))?;
    Ok(())
}
