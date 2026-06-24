//! SQLite implementation of the `MaintenanceStore` trait (Phase 30).

use arca_core::error::ArcaError;
use arca_core::store::maintenance::{
    MaintenanceJob, MaintenanceJobLog, MaintenanceJobStatus, MaintenanceStore,
};
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

const JOB_COLUMNS: &str =
    "id, type, status, mode, params, total, done, rate, last_error, created_at, updated_at, started_at, finished_at";

fn parse_dt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|v| v.parse::<DateTime<Utc>>().ok())
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<MaintenanceJob> {
    let params_str: String = row.get("params")?;
    Ok(MaintenanceJob {
        id: row.get("id")?,
        job_type: row.get("type")?,
        status: row.get("status")?,
        mode: row.get("mode")?,
        params: serde_json::from_str(&params_str).unwrap_or(serde_json::Value::Null),
        total: row.get::<_, i64>("total")? as u64,
        done: row.get::<_, i64>("done")? as u64,
        rate: row.get::<_, f64>("rate")?,
        last_error: row.get("last_error")?,
        created_at: row
            .get::<_, String>("created_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        updated_at: row
            .get::<_, String>("updated_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        started_at: parse_dt(row.get("started_at")?),
        finished_at: parse_dt(row.get("finished_at")?),
    })
}

#[async_trait::async_trait]
impl MaintenanceStore for SqliteStore {
    async fn create_job(&self, job: &MaintenanceJob) -> Result<(), ArcaError> {
        let j = job.clone();
        let params_str = serde_json::to_string(&j.params).unwrap_or_else(|_| "{}".to_string());
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO maintenance_jobs (
                        id, type, status, mode, params, total, done, rate,
                        last_error, created_at, updated_at, started_at, finished_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        j.id,
                        j.job_type,
                        j.status,
                        j.mode,
                        params_str,
                        j.total as i64,
                        j.done as i64,
                        j.rate,
                        j.last_error,
                        j.created_at.to_rfc3339(),
                        j.updated_at.to_rfc3339(),
                        j.started_at.map(|t| t.to_rfc3339()),
                        j.finished_at.map(|t| t.to_rfc3339()),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("create_job: {e}")))
    }

    async fn get_job(&self, id: &str) -> Result<Option<MaintenanceJob>, ArcaError> {
        let id = id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM maintenance_jobs WHERE id = ?1"
                ))?;
                let mut rows = stmt.query_map(params![id], row_to_job)?;
                match rows.next() {
                    Some(r) => Ok(Some(r?)),
                    None => Ok(None),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_job: {e}")))
    }

    async fn active_job(&self) -> Result<Option<MaintenanceJob>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM maintenance_jobs \
                     WHERE status IN ('pending','running','paused') \
                     ORDER BY created_at ASC LIMIT 1"
                ))?;
                let mut rows = stmt.query_map([], row_to_job)?;
                match rows.next() {
                    Some(r) => Ok(Some(r?)),
                    None => Ok(None),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("active_job: {e}")))
    }

    async fn list_jobs(&self, limit: u32) -> Result<Vec<MaintenanceJob>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM maintenance_jobs \
                     ORDER BY created_at DESC LIMIT ?1"
                ))?;
                let rows = stmt.query_map(params![limit], row_to_job)?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_jobs: {e}")))
    }

    async fn update_job_progress(
        &self,
        id: &str,
        done: u64,
        total: u64,
        rate: f64,
    ) -> Result<(), ArcaError> {
        let id = id.to_string();
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE maintenance_jobs SET done = ?1, total = ?2, rate = ?3, updated_at = ?4 \
                     WHERE id = ?5",
                    params![done as i64, total as i64, rate, now, id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_job_progress: {e}")))
    }

    async fn set_job_status(
        &self,
        id: &str,
        status: MaintenanceJobStatus,
        last_error: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let id = id.to_string();
        let status = status.as_db().to_string();
        let last_error = last_error.map(|s| s.to_string());
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                // ?1 status, ?2 last_error, ?3 now (also start/finish stamp), ?4 id.
                let rows = conn.execute(
                    "UPDATE maintenance_jobs SET \
                        status = ?1, \
                        last_error = ?2, \
                        updated_at = ?3, \
                        started_at = CASE WHEN ?1 = 'running' AND started_at IS NULL THEN ?3 ELSE started_at END, \
                        finished_at = CASE WHEN ?1 IN ('completed','failed','cancelled') THEN ?3 ELSE finished_at END \
                     WHERE id = ?4",
                    params![status, last_error, now, id],
                )?;
                Ok(rows > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("set_job_status: {e}")))
    }

    async fn append_job_log(
        &self,
        job_id: &str,
        level: &str,
        message: &str,
        max_logs: u32,
    ) -> Result<(), ArcaError> {
        let job_id = job_id.to_string();
        let level = level.to_string();
        let message = message.to_string();
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "INSERT INTO maintenance_job_logs (job_id, ts, level, message) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![job_id, now, level, message],
                )?;
                // Prune to the newest `max_logs` rows for this job (rowid is the
                // monotonic insert order, robust to equal timestamps).
                tx.execute(
                    "DELETE FROM maintenance_job_logs WHERE job_id = ?1 AND rowid NOT IN (
                        SELECT rowid FROM maintenance_job_logs WHERE job_id = ?1 \
                        ORDER BY rowid DESC LIMIT ?2
                     )",
                    params![job_id, max_logs],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("append_job_log: {e}")))
    }

    async fn list_job_logs(
        &self,
        job_id: &str,
        limit: u32,
    ) -> Result<Vec<MaintenanceJobLog>, ArcaError> {
        let job_id = job_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT job_id, ts, level, message FROM maintenance_job_logs \
                     WHERE job_id = ?1 ORDER BY rowid DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![job_id, limit], |row| {
                    Ok(MaintenanceJobLog {
                        job_id: row.get("job_id")?,
                        ts: row
                            .get::<_, String>("ts")?
                            .parse::<DateTime<Utc>>()
                            .unwrap_or_default(),
                        level: row.get("level")?,
                        message: row.get("message")?,
                    })
                })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_job_logs: {e}")))
    }

    async fn interrupt_running_jobs(&self) -> Result<u64, ArcaError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                let rows = conn.execute(
                    "UPDATE maintenance_jobs SET status = 'failed', \
                        last_error = 'interrupted by restart', \
                        updated_at = ?1, finished_at = ?1 \
                     WHERE status = 'running'",
                    params![now],
                )?;
                Ok(rows as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("interrupt_running_jobs: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::maintenance::MaintenanceJobStatus as St;

    async fn store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    fn job(id: &str, status: St, mode: &str) -> MaintenanceJob {
        let now = Utc::now();
        MaintenanceJob {
            id: id.to_string(),
            job_type: "noop".to_string(),
            status: status.as_db().to_string(),
            mode: mode.to_string(),
            params: serde_json::json!({"n": 5}),
            total: 5,
            done: 0,
            rate: 0.0,
            last_error: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn create_get_and_params_roundtrip() {
        let s = store().await;
        s.create_job(&job("j1", St::Pending, "live")).await.unwrap();
        let got = s.get_job("j1").await.unwrap().unwrap();
        assert_eq!(got.job_type, "noop");
        assert_eq!(got.status, "pending");
        assert_eq!(got.mode, "live");
        assert_eq!(got.params, serde_json::json!({"n": 5}));
        assert_eq!(got.total, 5);
        assert!(s.get_job("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn full_lifecycle_status_stamps_and_progress() {
        let s = store().await;
        s.create_job(&job("j1", St::Pending, "maintenance")).await.unwrap();

        // pending -> running stamps started_at, leaves finished_at null.
        assert!(s.set_job_status("j1", St::Running, None).await.unwrap());
        let r = s.get_job("j1").await.unwrap().unwrap();
        assert_eq!(r.status, "running");
        assert!(r.started_at.is_some());
        assert!(r.finished_at.is_none());

        // progress.
        s.update_job_progress("j1", 3, 5, 1.5).await.unwrap();
        let r = s.get_job("j1").await.unwrap().unwrap();
        assert_eq!(r.done, 3);
        assert_eq!(r.rate, 1.5);

        // pause then resume keeps started_at, no finished_at.
        assert!(s.set_job_status("j1", St::Paused, None).await.unwrap());
        let started = s.get_job("j1").await.unwrap().unwrap().started_at;
        assert!(s.set_job_status("j1", St::Running, None).await.unwrap());
        assert_eq!(s.get_job("j1").await.unwrap().unwrap().started_at, started);

        // terminal stamps finished_at.
        assert!(s.set_job_status("j1", St::Completed, None).await.unwrap());
        let r = s.get_job("j1").await.unwrap().unwrap();
        assert_eq!(r.status, "completed");
        assert!(r.finished_at.is_some());

        // status on a missing job returns false.
        assert!(!s.set_job_status("missing", St::Failed, Some("x")).await.unwrap());
    }

    #[tokio::test]
    async fn active_job_tracks_the_single_non_terminal_slot() {
        let s = store().await;
        assert!(s.active_job().await.unwrap().is_none());

        s.create_job(&job("j1", St::Pending, "live")).await.unwrap();
        assert_eq!(s.active_job().await.unwrap().unwrap().id, "j1");

        // Once terminal, the slot frees up.
        s.set_job_status("j1", St::Completed, None).await.unwrap();
        assert!(s.active_job().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn interrupt_running_jobs_fails_only_running() {
        let s = store().await;
        s.create_job(&job("running", St::Pending, "live")).await.unwrap();
        s.set_job_status("running", St::Running, None).await.unwrap();
        s.create_job(&job("paused", St::Pending, "live")).await.unwrap();
        s.set_job_status("paused", St::Paused, None).await.unwrap();

        assert_eq!(s.interrupt_running_jobs().await.unwrap(), 1);
        let r = s.get_job("running").await.unwrap().unwrap();
        assert_eq!(r.status, "failed");
        assert_eq!(r.last_error.as_deref(), Some("interrupted by restart"));
        // Paused jobs are untouched (operator resumes them).
        assert_eq!(s.get_job("paused").await.unwrap().unwrap().status, "paused");
    }

    #[tokio::test]
    async fn logs_append_prune_and_list_newest_first() {
        let s = store().await;
        s.create_job(&job("j1", St::Pending, "live")).await.unwrap();
        for i in 0..10 {
            s.append_job_log("j1", "info", &format!("line {i}"), 5)
                .await
                .unwrap();
        }
        let logs = s.list_job_logs("j1", 100).await.unwrap();
        // Pruned to the newest 5.
        assert_eq!(logs.len(), 5);
        // Newest first.
        assert_eq!(logs[0].message, "line 9");
        assert_eq!(logs[4].message, "line 5");
        assert_eq!(logs[0].level, "info");
    }
}
