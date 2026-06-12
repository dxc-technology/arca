//! PostgreSQL notification connector.
//!
//! Delivers S3 event notifications by inserting rows into a PostgreSQL table.
//! The target table is auto-created if it does not exist. Supports optional
//! `table` and `schema` properties.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use sqlx_core::connection::Connection;
use sqlx_core::executor::Executor;

use super::db_common;

/// PostgreSQL connector — delivers events via INSERT into a PostgreSQL table.
pub struct PostgresqlConnector {
    timeout: Duration,
}

impl PostgresqlConnector {
    /// Create a new PostgreSQL connector with the given connection timeout.
    pub fn new(timeout: Duration) -> Self {
        PostgresqlConnector { timeout }
    }

    /// Build the qualified table name, optionally prefixed with a schema.
    fn qualified_table(properties: &HashMap<String, String>) -> String {
        let table = db_common::table_name(properties);
        if let Some(schema) = properties.get("schema").filter(|s| !s.is_empty()) {
            format!("{schema}.{table}")
        } else {
            table.to_string()
        }
    }

    /// Open a single connection to the given PostgreSQL URL.
    async fn connect(
        &self,
        destination: &str,
    ) -> Result<sqlx_postgres::PgConnection, String> {
        tokio::time::timeout(
            self.timeout,
            sqlx_postgres::PgConnection::connect(destination),
        )
        .await
        .map_err(|_| "connection timeout".to_string())?
        .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl NotificationConnector for PostgresqlConnector {
    fn name(&self) -> &str {
        "postgresql"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let table = Self::qualified_table(properties);

        let fields = match db_common::extract_event_fields(payload) {
            Ok(f) => f,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "payload parse error".to_string(),
                    error: Some(e),
                };
            }
        };

        let mut conn = match self.connect(destination).await {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        // Ensure the target table exists.
        let ddl = db_common::create_table_ddl_postgres(&table);
        if let Err(e) = conn.execute(sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(ddl.as_str()))).await {
            return DeliveryResult {
                success: false,
                status_info: "DDL error".to_string(),
                error: Some(e.to_string()),
            };
        }

        // Insert the event row.
        let insert_sql = format!(
            "INSERT INTO {table} (id, event_name, bucket, key, event_time, payload) \
             VALUES ($1, $2, $3, $4, $5, $6)"
        );
        match sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(insert_sql.as_str()))
            .bind(&fields.id)
            .bind(&fields.event_name)
            .bind(&fields.bucket)
            .bind(&fields.key)
            .bind(fields.event_time)
            .bind(payload)
            .execute(&mut conn)
            .await
        {
            Ok(_) => DeliveryResult {
                success: true,
                status_info: format!("INSERT into '{table}'"),
                error: None,
            },
            Err(e) => DeliveryResult {
                success: false,
                status_info: "insert error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let table = Self::qualified_table(properties);

        let mut conn = match self.connect(destination).await {
            Ok(c) => c,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        // Ensure the target table exists.
        let ddl = db_common::create_table_ddl_postgres(&table);
        if let Err(e) = conn.execute(sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(ddl.as_str()))).await {
            return TestResult {
                success: false,
                status_info: "DDL error".to_string(),
                error: Some(e.to_string()),
            };
        }

        // Verify connectivity with a simple query.
        match conn.execute(sqlx_core::query::query("SELECT 1")).await {
            Ok(_) => TestResult {
                success: true,
                status_info: "OK".to_string(),
                error: None,
            },
            Err(e) => TestResult {
                success: false,
                status_info: "query error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qualified_table_default() {
        let props = HashMap::new();
        assert_eq!(
            PostgresqlConnector::qualified_table(&props),
            db_common::DEFAULT_TABLE_NAME
        );
    }

    #[test]
    fn test_qualified_table_with_schema() {
        let mut props = HashMap::new();
        props.insert("schema".to_string(), "notifications".to_string());
        assert_eq!(
            PostgresqlConnector::qualified_table(&props),
            format!("notifications.{}", db_common::DEFAULT_TABLE_NAME)
        );
    }

    #[test]
    fn test_qualified_table_custom_both() {
        let mut props = HashMap::new();
        props.insert("schema".to_string(), "ns".to_string());
        props.insert("table".to_string(), "events".to_string());
        assert_eq!(PostgresqlConnector::qualified_table(&props), "ns.events");
    }

    #[test]
    fn test_connector_name() {
        let connector = PostgresqlConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "postgresql");
    }

    #[tokio::test]
    async fn test_deliver_connection_refused() {
        let connector = PostgresqlConnector::new(Duration::from_secs(2));
        let props = HashMap::new();
        let payload = r#"{"Records":[{"eventName":"s3:ObjectCreated:Put","eventTime":"2026-01-01T00:00:00Z","s3":{"bucket":{"name":"b"},"object":{"key":"k"}}}]}"#;
        let result = connector
            .deliver("postgresql://127.0.0.1:1/test", payload, &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_test_connection_refused() {
        let connector = PostgresqlConnector::new(Duration::from_secs(2));
        let props = HashMap::new();
        let result = connector
            .test("postgresql://127.0.0.1:1/test", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
