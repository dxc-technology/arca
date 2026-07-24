//! PostgreSQL implementation of the `MaintenanceStore` trait (Phase 30).

use arca_core::error::ArcaError;
use arca_core::store::maintenance::{
    MaintenanceJob, MaintenanceJobLog, MaintenanceJobStatus, MaintenanceStore,
};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

const JOB_COLUMNS: &str =
    "id, type, status, mode, params, total, done, rate, last_error, created_at, updated_at, started_at, finished_at";

fn row_to_job(row: &sqlx_postgres::PgRow) -> MaintenanceJob {
    let params_str: String = row.get("params");
    MaintenanceJob {
        id: row.get("id"),
        job_type: row.get("type"),
        status: row.get("status"),
        mode: row.get("mode"),
        params: serde_json::from_str(&params_str).unwrap_or(serde_json::Value::Null),
        total: row.get::<i64, _>("total") as u64,
        done: row.get::<i64, _>("done") as u64,
        rate: row.get::<f64, _>("rate"),
        last_error: row.get("last_error"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
        started_at: row.get::<Option<DateTime<Utc>>, _>("started_at"),
        finished_at: row.get::<Option<DateTime<Utc>>, _>("finished_at"),
    }
}

#[async_trait::async_trait]
impl MaintenanceStore for PgStore {
    async fn create_job(&self, job: &MaintenanceJob) -> Result<(), ArcaError> {
        let params_str = serde_json::to_string(&job.params).unwrap_or_else(|_| "{}".to_string());
        sqlx_core::query::query(
            "INSERT INTO maintenance_jobs (
                id, type, status, mode, params, total, done, rate,
                last_error, created_at, updated_at, started_at, finished_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(&job.id)
        .bind(&job.job_type)
        .bind(&job.status)
        .bind(&job.mode)
        .bind(&params_str)
        .bind(job.total as i64)
        .bind(job.done as i64)
        .bind(job.rate)
        .bind(&job.last_error)
        .bind(job.created_at)
        .bind(job.updated_at)
        .bind(job.started_at)
        .bind(job.finished_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("create_job: {e}")))?;
        Ok(())
    }

    async fn get_job(&self, id: &str) -> Result<Option<MaintenanceJob>, ArcaError> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM maintenance_jobs WHERE id = $1");
        let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("get_job: {e}")))?;
        Ok(row.as_ref().map(row_to_job))
    }

    async fn active_job(&self) -> Result<Option<MaintenanceJob>, ArcaError> {
        let sql = format!(
            "SELECT {JOB_COLUMNS} FROM maintenance_jobs \
             WHERE status IN ('pending','running','paused') \
             ORDER BY created_at ASC LIMIT 1"
        );
        let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("active_job: {e}")))?;
        Ok(row.as_ref().map(row_to_job))
    }

    async fn list_jobs(&self, limit: u32) -> Result<Vec<MaintenanceJob>, ArcaError> {
        let sql =
            format!("SELECT {JOB_COLUMNS} FROM maintenance_jobs ORDER BY created_at DESC LIMIT $1");
        let rows = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_jobs: {e}")))?;
        Ok(rows.iter().map(row_to_job).collect())
    }

    async fn update_job_progress(
        &self,
        id: &str,
        done: u64,
        total: u64,
        rate: f64,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "UPDATE maintenance_jobs SET done = $1, total = $2, rate = $3, updated_at = $4 \
             WHERE id = $5",
        )
        .bind(done as i64)
        .bind(total as i64)
        .bind(rate)
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("update_job_progress: {e}")))?;
        Ok(())
    }

    async fn set_job_status(
        &self,
        id: &str,
        status: MaintenanceJobStatus,
        last_error: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let result = sqlx_core::query::query(
            "UPDATE maintenance_jobs SET \
                status = $1, \
                last_error = $2, \
                updated_at = $3, \
                started_at = CASE WHEN $1 = 'running' AND started_at IS NULL THEN $3 ELSE started_at END, \
                finished_at = CASE WHEN $1 IN ('completed','failed','cancelled') THEN $3 ELSE finished_at END \
             WHERE id = $4",
        )
        .bind(status.as_db())
        .bind(last_error)
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("set_job_status: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn append_job_log(
        &self,
        job_id: &str,
        level: &str,
        message: &str,
        max_logs: u32,
    ) -> Result<(), ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("append_job_log: {e}")))?;
        sqlx_core::query::query(
            "INSERT INTO maintenance_job_logs (job_id, ts, level, message) VALUES ($1, $2, $3, $4)",
        )
        .bind(job_id)
        .bind(Utc::now())
        .bind(level)
        .bind(message)
        .execute(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("append_job_log: {e}")))?;
        // Prune to the newest `max_logs` rows (seq is the monotonic insert order).
        sqlx_core::query::query(
            "DELETE FROM maintenance_job_logs WHERE job_id = $1 AND seq NOT IN (
                SELECT seq FROM maintenance_job_logs WHERE job_id = $1 ORDER BY seq DESC LIMIT $2
             )",
        )
        .bind(job_id)
        .bind(max_logs as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("append_job_log: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("append_job_log: {e}")))?;
        Ok(())
    }

    async fn list_job_logs(
        &self,
        job_id: &str,
        limit: u32,
    ) -> Result<Vec<MaintenanceJobLog>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT job_id, ts, level, message FROM maintenance_job_logs \
             WHERE job_id = $1 ORDER BY seq DESC LIMIT $2",
        )
        .bind(job_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_job_logs: {e}")))?;
        Ok(rows
            .iter()
            .map(|row| MaintenanceJobLog {
                job_id: row.get("job_id"),
                ts: row.get::<DateTime<Utc>, _>("ts"),
                level: row.get("level"),
                message: row.get("message"),
            })
            .collect())
    }

    async fn interrupt_running_jobs(&self) -> Result<u64, ArcaError> {
        let now = Utc::now();
        let result = sqlx_core::query::query(
            "UPDATE maintenance_jobs SET status = 'failed', \
                last_error = 'interrupted by restart', updated_at = $1, finished_at = $1 \
             WHERE status = 'running'",
        )
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("interrupt_running_jobs: {e}")))?;
        Ok(result.rows_affected())
    }

    async fn clear_terminal_jobs(&self) -> Result<u64, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("clear_terminal_jobs: {e}")))?;
        sqlx_core::query::query(
            "DELETE FROM maintenance_job_logs WHERE job_id IN (
                SELECT id FROM maintenance_jobs WHERE status IN ('completed','failed','cancelled')
             )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("clear_terminal_jobs: {e}")))?;
        let result = sqlx_core::query::query(
            "DELETE FROM maintenance_jobs WHERE status IN ('completed','failed','cancelled')",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("clear_terminal_jobs: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("clear_terminal_jobs: {e}")))?;
        Ok(result.rows_affected())
    }
}
