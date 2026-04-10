//! MongoDB notification connector.
//!
//! Delivers S3 event notifications by inserting documents into a MongoDB
//! collection. The target collection is auto-created on first insert (standard
//! MongoDB behavior). Supports optional `database` and `collection` properties.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use mongodb::bson::doc;

use super::db_common;

/// MongoDB connector — delivers events via INSERT into a MongoDB collection.
pub struct MongodbConnector {
    timeout: Duration,
}

impl MongodbConnector {
    /// Create a new MongoDB connector with the given connection timeout.
    pub fn new(timeout: Duration) -> Self {
        MongodbConnector { timeout }
    }

    /// Create a MongoDB client from the destination URL.
    async fn connect(&self, destination: &str) -> Result<mongodb::Client, String> {
        let mut options = mongodb::options::ClientOptions::parse(destination)
            .await
            .map_err(|e| e.to_string())?;

        options.connect_timeout = Some(self.timeout);
        options.server_selection_timeout = Some(self.timeout);

        mongodb::Client::with_options(options).map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl NotificationConnector for MongodbConnector {
    fn name(&self) -> &str {
        "mongodb"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let db_name = db_common::database_name(properties);
        let coll_name = db_common::collection_name(properties);

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

        let client = match self.connect(destination).await {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        let collection = client
            .database(db_name)
            .collection::<mongodb::bson::Document>(coll_name);

        let document = doc! {
            "_id": &fields.id,
            "event_name": &fields.event_name,
            "bucket": &fields.bucket,
            "key": &fields.key,
            "event_time": mongodb::bson::DateTime::from_millis(fields.event_time.timestamp_millis()),
            "payload": payload,
            "created_at": mongodb::bson::DateTime::now(),
        };

        match tokio::time::timeout(self.timeout, collection.insert_one(document)).await {
            Ok(Ok(_)) => DeliveryResult {
                success: true,
                status_info: format!("INSERT into '{db_name}.{coll_name}'"),
                error: None,
            },
            Ok(Err(e)) => DeliveryResult {
                success: false,
                status_info: "insert error".to_string(),
                error: Some(e.to_string()),
            },
            Err(_) => DeliveryResult {
                success: false,
                status_info: "insert timeout".to_string(),
                error: Some("operation timed out".to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let db_name = db_common::database_name(properties);

        let client = match self.connect(destination).await {
            Ok(c) => c,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        let db = client.database(db_name);
        match tokio::time::timeout(self.timeout, db.run_command(doc! { "ping": 1 })).await {
            Ok(Ok(_)) => TestResult {
                success: true,
                status_info: "OK".to_string(),
                error: None,
            },
            Ok(Err(e)) => TestResult {
                success: false,
                status_info: "ping error".to_string(),
                error: Some(e.to_string()),
            },
            Err(_) => TestResult {
                success: false,
                status_info: "connection timeout".to_string(),
                error: Some("operation timed out".to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_database_name_default() {
        let props = HashMap::new();
        assert_eq!(db_common::database_name(&props), db_common::DEFAULT_DATABASE_NAME);
    }

    #[test]
    fn test_database_name_custom() {
        let mut props = HashMap::new();
        props.insert("database".to_string(), "my_db".to_string());
        assert_eq!(db_common::database_name(&props), "my_db");
    }

    #[test]
    fn test_collection_name_default() {
        let props = HashMap::new();
        assert_eq!(db_common::collection_name(&props), db_common::DEFAULT_COLLECTION_NAME);
    }

    #[test]
    fn test_collection_name_custom() {
        let mut props = HashMap::new();
        props.insert("collection".to_string(), "events".to_string());
        assert_eq!(db_common::collection_name(&props), "events");
    }

    #[test]
    fn test_connector_name() {
        let connector = MongodbConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "mongodb");
    }

    #[tokio::test]
    async fn test_test_connection_refused() {
        let connector = MongodbConnector::new(Duration::from_secs(2));
        let props = HashMap::new();
        let result = connector
            .test("mongodb://127.0.0.1:1", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
