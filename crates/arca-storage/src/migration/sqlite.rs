//! SQLite read/write primitives for the generic backend migration.
//!
//! Rows are read as raw SQLite values via `ValueRef`, then reinterpreted per the
//! column's [`ColumnKind`] (a TEXT column may hold an RFC3339 timestamp or a JSON
//! document; an INTEGER column may hold a boolean 0/1). Writes bind a
//! `rusqlite::types::Value` produced from each [`Cell`] (timestamps → RFC3339
//! text, booleans → integer 0/1, JSON → its canonical text form).

use chrono::{DateTime, Utc};
use rusqlite::types::{Value, ValueRef};
use rusqlite::ToSql;

use super::{Cell, ColumnKind, TableDesc, BATCH_SIZE};
use crate::sqlite::{SqliteStore, TrError};

/// Converts a raw SQLite value to a [`Cell`], honoring the column's logical kind.
fn value_to_cell(v: ValueRef<'_>, kind: ColumnKind) -> Result<Cell, String> {
    match v {
        ValueRef::Null => Ok(Cell::Null),
        ValueRef::Integer(i) => match kind {
            ColumnKind::Bool => Ok(Cell::Bool(i != 0)),
            _ => Ok(Cell::Int(i)),
        },
        ValueRef::Real(f) => Ok(Cell::Float(f)),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            let s = std::str::from_utf8(bytes)
                .map_err(|e| format!("non-UTF8 text value: {e}"))?
                .to_string();
            match kind {
                ColumnKind::Timestamp => {
                    let ts = DateTime::parse_from_rfc3339(&s)
                        .map_err(|e| format!("invalid rfc3339 timestamp \"{s}\": {e}"))?
                        .with_timezone(&Utc);
                    Ok(Cell::Ts(ts))
                }
                ColumnKind::Json => {
                    let j: serde_json::Value = serde_json::from_str(&s)
                        .map_err(|e| format!("invalid JSON \"{s}\": {e}"))?;
                    Ok(Cell::Json(j))
                }
                _ => Ok(Cell::Text(s)),
            }
        }
    }
}

/// Converts a [`Cell`] into a SQLite-bindable value.
fn cell_to_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Int(i) => Value::Integer(*i),
        Cell::Float(f) => Value::Real(*f),
        Cell::Text(s) => Value::Text(s.clone()),
        Cell::Json(j) => Value::Text(j.to_string()),
        Cell::Bool(b) => Value::Integer(if *b { 1 } else { 0 }),
        Cell::Ts(ts) => Value::Text(ts.to_rfc3339()),
    }
}

/// Reads every row of `table` as ordered cells.
pub async fn dump_table(store: &SqliteStore, table: &TableDesc) -> Result<Vec<Vec<Cell>>, String> {
    let columns: Vec<(&'static str, ColumnKind)> =
        table.columns.iter().map(|c| (c.name, c.kind)).collect();
    let col_list = table
        .columns
        .iter()
        .map(|c| c.name)
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {col_list} FROM {}", table.name);

    store
        .read_conn()
        .call(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let mut cells = Vec::with_capacity(columns.len());
                for (idx, (_, kind)) in columns.iter().enumerate() {
                    let v = row.get_ref(idx)?;
                    let cell = value_to_cell(v, *kind).map_err(|e| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(
                            std::io::Error::other(e),
                        ))
                    })?;
                    cells.push(cell);
                }
                out.push(cells);
            }
            Ok(out)
        })
        .await
        .map_err(|e: TrError| format!("dump {}: {e}", table.name))
}

/// Inserts `rows` into `table` in batches.
pub async fn load_table(
    store: &SqliteStore,
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
    let table_name = table.name.to_string();

    // Modern bundled SQLite allows 32766 bound params per statement; clamp the
    // batch by that limit so wide tables never overflow it.
    let rows_per_batch = (32000 / num_cols.max(1)).min(BATCH_SIZE).max(1);
    for chunk in rows.chunks(rows_per_batch) {
        // Build a multi-row INSERT: VALUES (?,?,..),(?,?,..),...
        let row_placeholder = format!(
            "({})",
            (0..num_cols).map(|_| "?").collect::<Vec<_>>().join(", ")
        );
        let placeholders = (0..chunk.len())
            .map(|_| row_placeholder.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO {table_name} ({col_list}) VALUES {placeholders}");

        // Flatten the chunk's cells into a single bind list.
        let values: Vec<Value> = chunk
            .iter()
            .flat_map(|row| row.iter().map(cell_to_value))
            .collect();

        store.write_conn().call(move |conn| {
                let params: Vec<&dyn ToSql> = values.iter().map(|v| v as &dyn ToSql).collect();
                conn.execute(&sql, params.as_slice())?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| format!("load {table_name}: {e}"))?;
    }
    Ok(())
}

/// Number of rows in `table`.
pub async fn count(store: &SqliteStore, table: &str) -> Result<u64, String> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    store
        .read_conn()
        .call(move |conn| {
            let n: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
            Ok(n)
        })
        .await
        .map(|n| n as u64)
        .map_err(|e: TrError| format!("count {table}: {e}"))
}

/// Deletes every row from `table`.
pub async fn truncate(store: &SqliteStore, table: &str) -> Result<(), String> {
    let sql = format!("DELETE FROM {table}");
    store.write_conn().call(move |conn| {
            conn.execute(&sql, [])?;
            Ok(())
        })
        .await
        .map_err(|e: TrError| format!("truncate {table}: {e}"))
}
