//! Notification connector trait and registry.
//!
//! Defines the interface for notification delivery backends (webhook, Kafka, Redis, etc.)
//! and a registry that maps connector types to their implementations.

use std::collections::HashMap;
use std::sync::Arc;

use crate::s3::notification::ConnectorType;

/// Result of a delivery attempt to a notification connector.
#[derive(Debug, Clone)]
pub struct DeliveryResult {
    /// Whether delivery was successful.
    pub success: bool,
    /// Status information (e.g. "HTTP 200", "Kafka offset 42").
    pub status_info: String,
    /// Error message if delivery failed.
    pub error: Option<String>,
}

/// Result of a connectivity test to a notification connector.
#[derive(Debug, Clone)]
pub struct TestResult {
    /// Whether the test was successful.
    pub success: bool,
    /// Status information (e.g. "HTTP 200").
    pub status_info: String,
    /// Error message if the test failed.
    pub error: Option<String>,
}

/// Trait for notification delivery connectors.
///
/// Each connector type (webhook, Kafka, Redis, etc.) implements this trait.
/// The trait lives in `arca-core` following the same pattern as `BlobStore` and
/// `MetadataStore`: trait definition in core, implementations in server/storage crates.
///
/// Connectors are stateless with respect to individual destinations — a single
/// connector instance serves all destinations of that type concurrently.
#[async_trait::async_trait]
pub trait NotificationConnector: Send + Sync {
    /// Human-readable name for logging and UI display.
    fn name(&self) -> &str;

    /// Deliver a JSON payload to the given destination.
    ///
    /// - `destination`: the target address (URL for webhooks, broker for Kafka, etc.)
    /// - `payload`: serialized S3EventMessage JSON
    /// - `properties`: connector-specific settings from the destination config
    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult;

    /// Test connectivity to the given destination without persisting an event.
    ///
    /// Used by the admin API to verify that a connector configuration is correct.
    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult;
}

/// Registry of notification connectors, keyed by connector type.
///
/// Built once at startup, passed to the notification worker and admin handlers.
/// Only connector types that have been registered can deliver events; destinations
/// using an unregistered type will fail with a "connector not available" error.
pub struct ConnectorRegistry {
    connectors: HashMap<ConnectorType, Arc<dyn NotificationConnector>>,
}

impl ConnectorRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        ConnectorRegistry {
            connectors: HashMap::new(),
        }
    }

    /// Register a connector for the given type.
    pub fn register(
        &mut self,
        connector_type: ConnectorType,
        connector: Arc<dyn NotificationConnector>,
    ) {
        self.connectors.insert(connector_type, connector);
    }

    /// Look up a connector by type. Returns `None` if the type is not registered.
    pub fn get(&self, connector_type: &ConnectorType) -> Option<&Arc<dyn NotificationConnector>> {
        self.connectors.get(connector_type)
    }

    /// Returns true if the given connector type has been registered.
    pub fn is_available(&self, connector_type: &ConnectorType) -> bool {
        self.connectors.contains_key(connector_type)
    }

    /// List all registered connector types.
    pub fn available_types(&self) -> Vec<ConnectorType> {
        self.connectors.keys().copied().collect()
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}
