//! Shared utilities for database notification connectors (PostgreSQL, MySQL, MongoDB).
//!
//! Provides default names, DDL generation, and event field extraction that are
//! reused across the RDBMS and document-store connectors.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

/// Default table name for RDBMS notification event storage.
pub const DEFAULT_TABLE_NAME: &str = "arca_notifications";

/// Default MongoDB database name.
pub const DEFAULT_DATABASE_NAME: &str = "arca";

/// Default MongoDB collection name.
pub const DEFAULT_COLLECTION_NAME: &str = "arca_notifications";

/// Default Elasticsearch index name.
pub const DEFAULT_INDEX_NAME: &str = "arca-notifications";

/// Extract the table name from properties, defaulting to [`DEFAULT_TABLE_NAME`].
pub fn table_name(properties: &HashMap<String, String>) -> &str {
    properties
        .get("table")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TABLE_NAME)
}

/// Extract the MongoDB database name from properties, defaulting to [`DEFAULT_DATABASE_NAME`].
pub fn database_name(properties: &HashMap<String, String>) -> &str {
    properties
        .get("database")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_DATABASE_NAME)
}

/// Extract the MongoDB collection name from properties, defaulting to [`DEFAULT_COLLECTION_NAME`].
pub fn collection_name(properties: &HashMap<String, String>) -> &str {
    properties
        .get("collection")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_COLLECTION_NAME)
}

/// Extract the Elasticsearch index name from properties, defaulting to [`DEFAULT_INDEX_NAME`].
pub fn index_name(properties: &HashMap<String, String>) -> &str {
    properties
        .get("index")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_INDEX_NAME)
}

/// Build a `CREATE TABLE IF NOT EXISTS` statement for PostgreSQL.
pub fn create_table_ddl_postgres(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (\
            id         TEXT PRIMARY KEY, \
            event_name TEXT NOT NULL, \
            bucket     TEXT NOT NULL, \
            key        TEXT NOT NULL, \
            event_time TIMESTAMPTZ NOT NULL, \
            payload    TEXT NOT NULL, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()\
        )"
    )
}

/// Build a `CREATE TABLE IF NOT EXISTS` statement for MySQL.
pub fn create_table_ddl_mysql(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (\
            id         VARCHAR(255) PRIMARY KEY, \
            event_name VARCHAR(255) NOT NULL, \
            bucket     VARCHAR(255) NOT NULL, \
            `key`      TEXT NOT NULL, \
            event_time DATETIME(6) NOT NULL, \
            payload    LONGTEXT NOT NULL, \
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)\
        )"
    )
}

/// Denormalized fields extracted from an S3 event payload.
pub struct EventFields {
    pub id: String,
    pub event_name: String,
    pub bucket: String,
    pub key: String,
    pub event_time: DateTime<Utc>,
}

/// Parse the first record from an S3 event JSON payload and extract key fields.
pub fn extract_event_fields(payload: &str) -> Result<EventFields, String> {
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| format!("invalid JSON payload: {e}"))?;

    let record = value
        .get("Records")
        .and_then(|r| r.get(0))
        .ok_or_else(|| "missing Records[0] in payload".to_string())?;

    let event_name = record
        .get("eventName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bucket = record
        .pointer("/s3/bucket/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let key = record
        .pointer("/s3/object/key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let event_time_str = record
        .get("eventTime")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let event_time = chrono::DateTime::parse_from_rfc3339(event_time_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());

    Ok(EventFields {
        id: uuid::Uuid::new_v4().to_string(),
        event_name,
        bucket,
        key,
        event_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_name_default() {
        let props = HashMap::new();
        assert_eq!(table_name(&props), DEFAULT_TABLE_NAME);
    }

    #[test]
    fn test_table_name_custom() {
        let mut props = HashMap::new();
        props.insert("table".to_string(), "my_events".to_string());
        assert_eq!(table_name(&props), "my_events");
    }

    #[test]
    fn test_table_name_empty_falls_back() {
        let mut props = HashMap::new();
        props.insert("table".to_string(), "".to_string());
        assert_eq!(table_name(&props), DEFAULT_TABLE_NAME);
    }

    #[test]
    fn test_database_name_default() {
        let props = HashMap::new();
        assert_eq!(database_name(&props), DEFAULT_DATABASE_NAME);
    }

    #[test]
    fn test_collection_name_default() {
        let props = HashMap::new();
        assert_eq!(collection_name(&props), DEFAULT_COLLECTION_NAME);
    }

    #[test]
    fn test_index_name_default() {
        let props = HashMap::new();
        assert_eq!(index_name(&props), DEFAULT_INDEX_NAME);
    }

    #[test]
    fn test_index_name_custom() {
        let mut props = HashMap::new();
        props.insert("index".to_string(), "my-index".to_string());
        assert_eq!(index_name(&props), "my-index");
    }

    #[test]
    fn test_index_name_empty_falls_back() {
        let mut props = HashMap::new();
        props.insert("index".to_string(), "".to_string());
        assert_eq!(index_name(&props), DEFAULT_INDEX_NAME);
    }

    #[test]
    fn test_postgres_ddl_contains_table_name() {
        let ddl = create_table_ddl_postgres("my_table");
        assert!(ddl.contains("my_table"));
        assert!(ddl.contains("TIMESTAMPTZ"));
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
    }

    #[test]
    fn test_mysql_ddl_contains_table_name() {
        let ddl = create_table_ddl_mysql("my_table");
        assert!(ddl.contains("my_table"));
        assert!(ddl.contains("DATETIME(6)"));
        assert!(ddl.contains("`key`"));
        assert!(ddl.contains("LONGTEXT"));
    }

    #[test]
    fn test_extract_event_fields() {
        let payload = r#"{"Records":[{
            "eventName":"s3:ObjectCreated:Put",
            "eventTime":"2026-01-15T10:30:00Z",
            "s3":{"bucket":{"name":"my-bucket"},"object":{"key":"my-key"}}
        }]}"#;
        let fields = extract_event_fields(payload).unwrap();
        assert_eq!(fields.event_name, "s3:ObjectCreated:Put");
        assert_eq!(fields.bucket, "my-bucket");
        assert_eq!(fields.key, "my-key");
        assert!(!fields.id.is_empty());
    }

    #[test]
    fn test_extract_event_fields_invalid_json() {
        let result = extract_event_fields("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_event_fields_missing_records() {
        let result = extract_event_fields("{}");
        assert!(result.is_err());
    }
}
