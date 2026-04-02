//! S3 Bucket Notification Configuration types, XML parsing, and serialization.
//!
//! Supports all three S3 destination types (TopicConfiguration, QueueConfiguration,
//! CloudFunctionConfiguration), treating them uniformly as webhook destinations.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;
use serde::{Deserialize, Serialize};

use crate::error::{write_xml_element, S3Error, S3ErrorCode};

// ── Configuration types (serde-serializable for JSON storage in bucket_config) ──

/// Complete notification configuration for a bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NotificationConfiguration {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topic_configurations: Vec<DestinationConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_configurations: Vec<DestinationConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cloud_function_configurations: Vec<DestinationConfig>,
}

impl NotificationConfiguration {
    /// Returns true if there are no destination configurations at all.
    pub fn is_empty(&self) -> bool {
        self.topic_configurations.is_empty()
            && self.queue_configurations.is_empty()
            && self.cloud_function_configurations.is_empty()
    }

    /// Iterate over all destination configs across all three types.
    pub fn all_configs(&self) -> impl Iterator<Item = &DestinationConfig> {
        self.topic_configurations
            .iter()
            .chain(self.queue_configurations.iter())
            .chain(self.cloud_function_configurations.iter())
    }
}

/// The S3 destination type (determines XML element name).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DestinationType {
    Topic,
    Queue,
    CloudFunction,
}

/// A single destination configuration (shared structure across all three S3 types).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DestinationConfig {
    pub id: String,
    /// Destination type — determines which XML element to use on serialization.
    pub destination_type: DestinationType,
    /// Destination URL (where webhook POSTs will be sent).
    /// In S3 this would be an ARN; we accept URLs directly.
    pub arn: String,
    /// Event types to match (e.g. `["s3:ObjectCreated:*"]`).
    pub events: Vec<String>,
    /// Optional key filter (prefix and/or suffix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<NotificationFilter>,
}

/// S3 key-name notification filter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotificationFilter {
    pub key: S3KeyFilter,
}

/// Key-based filter rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct S3KeyFilter {
    pub filter_rules: Vec<FilterRule>,
}

/// A single filter rule (prefix or suffix).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilterRule {
    /// `"prefix"` or `"suffix"`.
    pub name: String,
    pub value: String,
}

// ── Event types (JSON for webhook payload and DB storage) ──

/// S3 event notification message (the JSON body POSTed to webhooks).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct S3EventMessage {
    #[serde(rename = "Records")]
    pub records: Vec<S3EventRecord>,
}

/// A single S3 event record within a notification message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3EventRecord {
    pub event_version: String,
    pub event_source: String,
    #[serde(rename = "awsRegion")]
    pub region: String,
    pub event_time: String,
    pub event_name: String,
    pub user_identity: EventUserIdentity,
    pub request_parameters: EventRequestParameters,
    pub response_elements: EventResponseElements,
    pub s3: S3EventData,
}

/// User identity within an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventUserIdentity {
    pub principal_id: String,
}

/// Request parameters within an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventRequestParameters {
    pub source_ip_address: String,
}

/// Response elements within an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventResponseElements {
    #[serde(rename = "x-amz-request-id")]
    pub request_id: String,
    #[serde(rename = "x-amz-id-2")]
    pub id2: String,
}

/// S3-specific data within an event record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3EventData {
    pub s3_schema_version: String,
    pub configuration_id: String,
    pub bucket: S3EventBucket,
    pub object: S3EventObject,
}

/// Bucket information within an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3EventBucket {
    pub name: String,
    pub owner_identity: EventUserIdentity,
    pub arn: String,
}

/// Object information within an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3EventObject {
    pub key: String,
    pub size: u64,
    #[serde(rename = "eTag")]
    pub etag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    pub sequencer: String,
}

// ── Lightweight event type for the internal mpsc channel ──

/// Lightweight event emitted by handlers and sent through the mpsc channel.
/// The notification worker expands this into full S3EventRecord(s) for delivery.
#[derive(Debug, Clone)]
pub struct S3Event {
    pub event_name: String,
    pub bucket: String,
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub version_id: Option<String>,
    pub sequencer: String,
    pub user_identity: Option<String>,
    pub source_ip: Option<String>,
    pub request_id: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl S3Event {
    /// Build a full S3EventRecord for a given destination configuration ID and region.
    pub fn to_record(&self, configuration_id: &str, region: &str) -> S3EventRecord {
        S3EventRecord {
            event_version: "2.1".to_string(),
            event_source: "arca:s3".to_string(),
            region: region.to_string(),
            event_time: self.timestamp.to_rfc3339(),
            event_name: self.event_name.clone(),
            user_identity: EventUserIdentity {
                principal_id: self.user_identity.clone().unwrap_or_default(),
            },
            request_parameters: EventRequestParameters {
                source_ip_address: self.source_ip.clone().unwrap_or_default(),
            },
            response_elements: EventResponseElements {
                request_id: self.request_id.clone().unwrap_or_default(),
                id2: String::new(),
            },
            s3: S3EventData {
                s3_schema_version: "1.0".to_string(),
                configuration_id: configuration_id.to_string(),
                bucket: S3EventBucket {
                    name: self.bucket.clone(),
                    owner_identity: EventUserIdentity {
                        principal_id: String::new(),
                    },
                    arn: format!("arn:arca:s3:::{}", self.bucket),
                },
                object: S3EventObject {
                    key: self.key.clone(),
                    size: self.size,
                    etag: self.etag.clone(),
                    version_id: self.version_id.clone(),
                    sequencer: self.sequencer.clone(),
                },
            },
        }
    }
}

// ── Event matching helpers ──

/// Valid S3 event name prefixes.
const VALID_EVENT_PREFIXES: &[&str] = &[
    "s3:ObjectCreated:*",
    "s3:ObjectCreated:Put",
    "s3:ObjectCreated:Post",
    "s3:ObjectCreated:Copy",
    "s3:ObjectCreated:CompleteMultipartUpload",
    "s3:ObjectRemoved:*",
    "s3:ObjectRemoved:Delete",
    "s3:ObjectRemoved:DeleteMarkerCreated",
];

/// Check if an event name is a known S3 event type.
pub fn is_valid_event_name(name: &str) -> bool {
    VALID_EVENT_PREFIXES.contains(&name)
}

/// Check if a concrete event name matches a pattern (which may use `*` wildcard).
///
/// Examples:
/// - `matches_event("s3:ObjectCreated:Put", "s3:ObjectCreated:*")` → true
/// - `matches_event("s3:ObjectCreated:Put", "s3:ObjectCreated:Put")` → true
/// - `matches_event("s3:ObjectRemoved:Delete", "s3:ObjectCreated:*")` → false
pub fn matches_event(event_name: &str, pattern: &str) -> bool {
    if pattern == event_name {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return event_name.starts_with(prefix);
    }
    false
}

/// Check if an object key matches a notification filter's prefix/suffix rules.
pub fn matches_filter(key: &str, filter: &Option<NotificationFilter>) -> bool {
    let filter = match filter {
        Some(f) => f,
        None => return true, // No filter = match everything
    };

    for rule in &filter.key.filter_rules {
        match rule.name.to_lowercase().as_str() {
            "prefix" => {
                if !key.starts_with(&rule.value) {
                    return false;
                }
            }
            "suffix" => {
                if !key.ends_with(&rule.value) {
                    return false;
                }
            }
            _ => {} // Ignore unknown filter rule names
        }
    }
    true
}

// ── XML parsing ──

/// Parse an S3 NotificationConfiguration XML request body.
pub fn parse_notification_configuration_xml(
    xml: &str,
) -> Result<NotificationConfiguration, S3Error> {
    // Local serde structs for XML deserialization.
    #[derive(Deserialize)]
    #[serde(rename = "NotificationConfiguration")]
    struct XmlNotificationConfiguration {
        #[serde(rename = "TopicConfiguration", default)]
        topic_configurations: Vec<XmlDestinationConfig>,
        #[serde(rename = "QueueConfiguration", default)]
        queue_configurations: Vec<XmlDestinationConfig>,
        #[serde(rename = "CloudFunctionConfiguration", default)]
        cloud_function_configurations: Vec<XmlDestinationConfig>,
    }

    #[derive(Deserialize)]
    struct XmlDestinationConfig {
        #[serde(rename = "Id", default)]
        id: Option<String>,
        // S3 uses Topic/Queue/CloudFunction as the ARN element depending on type.
        // We try all three and take whichever is present.
        #[serde(rename = "Topic")]
        topic: Option<String>,
        #[serde(rename = "Queue")]
        queue: Option<String>,
        #[serde(rename = "CloudFunction")]
        cloud_function: Option<String>,
        #[serde(rename = "Event", default)]
        events: Vec<String>,
        #[serde(rename = "Filter")]
        filter: Option<XmlFilter>,
    }

    #[derive(Deserialize)]
    struct XmlFilter {
        #[serde(rename = "S3Key")]
        s3_key: Option<XmlS3Key>,
    }

    #[derive(Deserialize)]
    struct XmlS3Key {
        #[serde(rename = "FilterRule", default)]
        filter_rules: Vec<XmlFilterRule>,
    }

    #[derive(Deserialize)]
    struct XmlFilterRule {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "Value")]
        value: String,
    }

    let parsed: XmlNotificationConfiguration = quick_xml::de::from_str(xml).map_err(|e| {
        S3Error::with_message(
            S3ErrorCode::MalformedXML,
            format!("Invalid notification configuration XML: {e}"),
            "",
        )
    })?;

    fn convert_configs(
        xml_configs: Vec<XmlDestinationConfig>,
        dest_type: DestinationType,
    ) -> Result<Vec<DestinationConfig>, S3Error> {
        let mut result = Vec::with_capacity(xml_configs.len());
        for cfg in xml_configs {
            let arn = cfg
                .topic
                .or(cfg.queue)
                .or(cfg.cloud_function)
                .unwrap_or_default();

            if arn.is_empty() {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "Destination ARN/URL must not be empty",
                    "",
                ));
            }

            let events = cfg.events;
            if events.is_empty() {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "At least one Event must be specified",
                    "",
                ));
            }

            for ev in &events {
                if !is_valid_event_name(ev) {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InvalidArgument,
                        format!("Invalid event type: '{ev}'"),
                        "",
                    ));
                }
            }

            let filter = match cfg.filter {
                Some(f) => match f.s3_key {
                    Some(s3k) => {
                        let filter_rules: Vec<FilterRule> = s3k
                            .filter_rules
                            .into_iter()
                            .map(|r| FilterRule {
                                name: r.name,
                                value: r.value,
                            })
                            .collect();
                        // Validate filter rule names
                        for rule in &filter_rules {
                            let lower = rule.name.to_lowercase();
                            if lower != "prefix" && lower != "suffix" {
                                return Err(S3Error::with_message(
                                    S3ErrorCode::InvalidArgument,
                                    format!(
                                        "Invalid filter rule name: '{}'. Must be 'prefix' or 'suffix'",
                                        rule.name
                                    ),
                                    "",
                                ));
                            }
                        }
                        Some(NotificationFilter {
                            key: S3KeyFilter { filter_rules },
                        })
                    }
                    None => None,
                },
                None => None,
            };

            let id = cfg.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            result.push(DestinationConfig {
                id,
                destination_type: dest_type,
                arn,
                events,
                filter,
            });
        }
        Ok(result)
    }

    let topic_configurations =
        convert_configs(parsed.topic_configurations, DestinationType::Topic)?;
    let queue_configurations =
        convert_configs(parsed.queue_configurations, DestinationType::Queue)?;
    let cloud_function_configurations = convert_configs(
        parsed.cloud_function_configurations,
        DestinationType::CloudFunction,
    )?;

    let config = NotificationConfiguration {
        topic_configurations,
        queue_configurations,
        cloud_function_configurations,
    };

    validate_notification_configuration(&config)?;
    Ok(config)
}

/// Validate notification configuration constraints.
fn validate_notification_configuration(config: &NotificationConfiguration) -> Result<(), S3Error> {
    // Check for duplicate IDs across all configuration types
    let mut seen_ids = std::collections::HashSet::new();
    for cfg in config.all_configs() {
        if !cfg.id.is_empty() && !seen_ids.insert(&cfg.id) {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!("Duplicate configuration ID: '{}'", cfg.id),
                "",
            ));
        }
    }
    Ok(())
}

// ── XML serialization ──

/// Serialize a NotificationConfiguration to S3-compatible XML.
pub fn notification_configuration_to_xml(config: &NotificationConfiguration) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("NotificationConfiguration");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write NotificationConfiguration start");

    for cfg in &config.topic_configurations {
        write_destination_config_xml(&mut writer, cfg, "TopicConfiguration", "Topic");
    }
    for cfg in &config.queue_configurations {
        write_destination_config_xml(&mut writer, cfg, "QueueConfiguration", "Queue");
    }
    for cfg in &config.cloud_function_configurations {
        write_destination_config_xml(
            &mut writer,
            cfg,
            "CloudFunctionConfiguration",
            "CloudFunction",
        );
    }

    writer
        .write_event(Event::End(BytesEnd::new("NotificationConfiguration")))
        .expect("write NotificationConfiguration end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Write a single destination configuration element.
fn write_destination_config_xml(
    writer: &mut Writer<Vec<u8>>,
    cfg: &DestinationConfig,
    wrapper_tag: &str,
    arn_tag: &str,
) {
    writer
        .write_event(Event::Start(BytesStart::new(wrapper_tag)))
        .expect("write config start");

    write_xml_element(writer, "Id", &cfg.id);
    write_xml_element(writer, arn_tag, &cfg.arn);

    for event in &cfg.events {
        write_xml_element(writer, "Event", event);
    }

    if let Some(ref filter) = cfg.filter {
        writer
            .write_event(Event::Start(BytesStart::new("Filter")))
            .expect("write Filter start");
        writer
            .write_event(Event::Start(BytesStart::new("S3Key")))
            .expect("write S3Key start");

        for rule in &filter.key.filter_rules {
            writer
                .write_event(Event::Start(BytesStart::new("FilterRule")))
                .expect("write FilterRule start");
            write_xml_element(writer, "Name", &rule.name);
            write_xml_element(writer, "Value", &rule.value);
            writer
                .write_event(Event::End(BytesEnd::new("FilterRule")))
                .expect("write FilterRule end");
        }

        writer
            .write_event(Event::End(BytesEnd::new("S3Key")))
            .expect("write S3Key end");
        writer
            .write_event(Event::End(BytesEnd::new("Filter")))
            .expect("write Filter end");
    }

    writer
        .write_event(Event::End(BytesEnd::new(wrapper_tag)))
        .expect("write config end");
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_topic_configuration() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>hook1</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        assert_eq!(config.topic_configurations.len(), 1);
        assert!(config.queue_configurations.is_empty());
        assert!(config.cloud_function_configurations.is_empty());

        let cfg = &config.topic_configurations[0];
        assert_eq!(cfg.id, "hook1");
        assert_eq!(cfg.arn, "http://example.com/webhook");
        assert_eq!(cfg.events, vec!["s3:ObjectCreated:*"]);
        assert_eq!(cfg.destination_type, DestinationType::Topic);
        assert!(cfg.filter.is_none());
    }

    #[test]
    fn parse_queue_configuration() {
        let xml = r#"<NotificationConfiguration>
  <QueueConfiguration>
    <Id>q1</Id>
    <Queue>http://example.com/queue</Queue>
    <Event>s3:ObjectRemoved:*</Event>
  </QueueConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        assert_eq!(config.queue_configurations.len(), 1);
        let cfg = &config.queue_configurations[0];
        assert_eq!(cfg.destination_type, DestinationType::Queue);
        assert_eq!(cfg.arn, "http://example.com/queue");
    }

    #[test]
    fn parse_cloud_function_configuration() {
        let xml = r#"<NotificationConfiguration>
  <CloudFunctionConfiguration>
    <Id>cf1</Id>
    <CloudFunction>http://example.com/lambda</CloudFunction>
    <Event>s3:ObjectCreated:Put</Event>
  </CloudFunctionConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        assert_eq!(config.cloud_function_configurations.len(), 1);
        let cfg = &config.cloud_function_configurations[0];
        assert_eq!(cfg.destination_type, DestinationType::CloudFunction);
    }

    #[test]
    fn parse_multiple_configurations() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>t1</Id>
    <Topic>http://example.com/topic</Topic>
    <Event>s3:ObjectCreated:*</Event>
  </TopicConfiguration>
  <QueueConfiguration>
    <Id>q1</Id>
    <Queue>http://example.com/queue</Queue>
    <Event>s3:ObjectRemoved:*</Event>
  </QueueConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        assert_eq!(config.topic_configurations.len(), 1);
        assert_eq!(config.queue_configurations.len(), 1);
        assert!(!config.is_empty());
    }

    #[test]
    fn parse_with_prefix_filter() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>filtered</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
    <Filter>
      <S3Key>
        <FilterRule>
          <Name>prefix</Name>
          <Value>images/</Value>
        </FilterRule>
      </S3Key>
    </Filter>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        let cfg = &config.topic_configurations[0];
        let filter = cfg.filter.as_ref().unwrap();
        assert_eq!(filter.key.filter_rules.len(), 1);
        assert_eq!(filter.key.filter_rules[0].name, "prefix");
        assert_eq!(filter.key.filter_rules[0].value, "images/");
    }

    #[test]
    fn parse_with_suffix_filter() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>suffix-filter</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:Put</Event>
    <Filter>
      <S3Key>
        <FilterRule>
          <Name>suffix</Name>
          <Value>.jpg</Value>
        </FilterRule>
      </S3Key>
    </Filter>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        let filter = config.topic_configurations[0].filter.as_ref().unwrap();
        assert_eq!(filter.key.filter_rules[0].name, "suffix");
        assert_eq!(filter.key.filter_rules[0].value, ".jpg");
    }

    #[test]
    fn parse_with_prefix_and_suffix_filter() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>both-filters</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
    <Filter>
      <S3Key>
        <FilterRule>
          <Name>prefix</Name>
          <Value>photos/</Value>
        </FilterRule>
        <FilterRule>
          <Name>suffix</Name>
          <Value>.png</Value>
        </FilterRule>
      </S3Key>
    </Filter>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        let filter = config.topic_configurations[0].filter.as_ref().unwrap();
        assert_eq!(filter.key.filter_rules.len(), 2);
    }

    #[test]
    fn parse_multiple_events() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>multi-event</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:Put</Event>
    <Event>s3:ObjectCreated:Copy</Event>
    <Event>s3:ObjectRemoved:Delete</Event>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        assert_eq!(config.topic_configurations[0].events.len(), 3);
    }

    #[test]
    fn parse_empty_configuration() {
        let xml =
            r#"<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"></NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        assert!(config.is_empty());
    }

    #[test]
    fn parse_auto_generates_id_when_missing() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let config = parse_notification_configuration_xml(xml).unwrap();
        // ID should be auto-generated (UUID)
        assert!(!config.topic_configurations[0].id.is_empty());
    }

    #[test]
    fn reject_empty_arn() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>bad</Id>
    <Topic></Topic>
    <Event>s3:ObjectCreated:*</Event>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let err = parse_notification_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("must not be empty"));
    }

    #[test]
    fn reject_no_events() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>no-events</Id>
    <Topic>http://example.com/webhook</Topic>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let err = parse_notification_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("Event"));
    }

    #[test]
    fn reject_invalid_event_name() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>bad-event</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:InvalidEvent:Foo</Event>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let err = parse_notification_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("Invalid event type"));
    }

    #[test]
    fn reject_duplicate_ids() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>same-id</Id>
    <Topic>http://example.com/a</Topic>
    <Event>s3:ObjectCreated:*</Event>
  </TopicConfiguration>
  <QueueConfiguration>
    <Id>same-id</Id>
    <Queue>http://example.com/b</Queue>
    <Event>s3:ObjectRemoved:*</Event>
  </QueueConfiguration>
</NotificationConfiguration>"#;

        let err = parse_notification_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("Duplicate configuration ID"));
    }

    #[test]
    fn reject_invalid_filter_rule_name() {
        let xml = r#"<NotificationConfiguration>
  <TopicConfiguration>
    <Id>bad-filter</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
    <Filter>
      <S3Key>
        <FilterRule>
          <Name>regex</Name>
          <Value>.*</Value>
        </FilterRule>
      </S3Key>
    </Filter>
  </TopicConfiguration>
</NotificationConfiguration>"#;

        let err = parse_notification_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("prefix"));
    }

    #[test]
    fn reject_malformed_xml() {
        let err = parse_notification_configuration_xml("not valid xml").unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn xml_roundtrip_topic() {
        let config = NotificationConfiguration {
            topic_configurations: vec![DestinationConfig {
                id: "hook1".to_string(),
                destination_type: DestinationType::Topic,
                arn: "http://example.com/webhook".to_string(),
                events: vec![
                    "s3:ObjectCreated:*".to_string(),
                    "s3:ObjectRemoved:*".to_string(),
                ],
                filter: Some(NotificationFilter {
                    key: S3KeyFilter {
                        filter_rules: vec![FilterRule {
                            name: "prefix".to_string(),
                            value: "images/".to_string(),
                        }],
                    },
                }),
            }],
            queue_configurations: vec![],
            cloud_function_configurations: vec![],
        };

        let xml = notification_configuration_to_xml(&config);
        let parsed = parse_notification_configuration_xml(&xml).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn xml_roundtrip_all_types() {
        let config = NotificationConfiguration {
            topic_configurations: vec![DestinationConfig {
                id: "t1".to_string(),
                destination_type: DestinationType::Topic,
                arn: "http://example.com/topic".to_string(),
                events: vec!["s3:ObjectCreated:Put".to_string()],
                filter: None,
            }],
            queue_configurations: vec![DestinationConfig {
                id: "q1".to_string(),
                destination_type: DestinationType::Queue,
                arn: "http://example.com/queue".to_string(),
                events: vec!["s3:ObjectRemoved:Delete".to_string()],
                filter: None,
            }],
            cloud_function_configurations: vec![DestinationConfig {
                id: "cf1".to_string(),
                destination_type: DestinationType::CloudFunction,
                arn: "http://example.com/lambda".to_string(),
                events: vec!["s3:ObjectCreated:Copy".to_string()],
                filter: Some(NotificationFilter {
                    key: S3KeyFilter {
                        filter_rules: vec![
                            FilterRule {
                                name: "prefix".to_string(),
                                value: "docs/".to_string(),
                            },
                            FilterRule {
                                name: "suffix".to_string(),
                                value: ".pdf".to_string(),
                            },
                        ],
                    },
                }),
            }],
        };

        let xml = notification_configuration_to_xml(&config);
        let parsed = parse_notification_configuration_xml(&xml).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn xml_empty_configuration_roundtrip() {
        let config = NotificationConfiguration::default();
        let xml = notification_configuration_to_xml(&config);
        let parsed = parse_notification_configuration_xml(&xml).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn json_roundtrip() {
        let config = NotificationConfiguration {
            topic_configurations: vec![DestinationConfig {
                id: "hook1".to_string(),
                destination_type: DestinationType::Topic,
                arn: "http://example.com/webhook".to_string(),
                events: vec!["s3:ObjectCreated:*".to_string()],
                filter: None,
            }],
            queue_configurations: vec![],
            cloud_function_configurations: vec![],
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: NotificationConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn matches_event_exact() {
        assert!(matches_event(
            "s3:ObjectCreated:Put",
            "s3:ObjectCreated:Put"
        ));
        assert!(!matches_event(
            "s3:ObjectCreated:Put",
            "s3:ObjectCreated:Copy"
        ));
    }

    #[test]
    fn matches_event_wildcard() {
        assert!(matches_event(
            "s3:ObjectCreated:Put",
            "s3:ObjectCreated:*"
        ));
        assert!(matches_event(
            "s3:ObjectCreated:Copy",
            "s3:ObjectCreated:*"
        ));
        assert!(matches_event(
            "s3:ObjectCreated:CompleteMultipartUpload",
            "s3:ObjectCreated:*"
        ));
        assert!(!matches_event(
            "s3:ObjectRemoved:Delete",
            "s3:ObjectCreated:*"
        ));
    }

    #[test]
    fn matches_event_removed_wildcard() {
        assert!(matches_event(
            "s3:ObjectRemoved:Delete",
            "s3:ObjectRemoved:*"
        ));
        assert!(matches_event(
            "s3:ObjectRemoved:DeleteMarkerCreated",
            "s3:ObjectRemoved:*"
        ));
        assert!(!matches_event(
            "s3:ObjectCreated:Put",
            "s3:ObjectRemoved:*"
        ));
    }

    #[test]
    fn matches_filter_no_filter() {
        assert!(matches_filter("any/key.txt", &None));
    }

    #[test]
    fn matches_filter_prefix() {
        let filter = Some(NotificationFilter {
            key: S3KeyFilter {
                filter_rules: vec![FilterRule {
                    name: "prefix".to_string(),
                    value: "images/".to_string(),
                }],
            },
        });

        assert!(matches_filter("images/photo.jpg", &filter));
        assert!(!matches_filter("docs/readme.md", &filter));
    }

    #[test]
    fn matches_filter_suffix() {
        let filter = Some(NotificationFilter {
            key: S3KeyFilter {
                filter_rules: vec![FilterRule {
                    name: "suffix".to_string(),
                    value: ".jpg".to_string(),
                }],
            },
        });

        assert!(matches_filter("images/photo.jpg", &filter));
        assert!(!matches_filter("images/photo.png", &filter));
    }

    #[test]
    fn matches_filter_prefix_and_suffix() {
        let filter = Some(NotificationFilter {
            key: S3KeyFilter {
                filter_rules: vec![
                    FilterRule {
                        name: "prefix".to_string(),
                        value: "images/".to_string(),
                    },
                    FilterRule {
                        name: "suffix".to_string(),
                        value: ".jpg".to_string(),
                    },
                ],
            },
        });

        assert!(matches_filter("images/photo.jpg", &filter));
        assert!(!matches_filter("images/photo.png", &filter)); // suffix mismatch
        assert!(!matches_filter("docs/photo.jpg", &filter)); // prefix mismatch
        assert!(!matches_filter("docs/readme.md", &filter)); // both mismatch
    }

    #[test]
    fn is_valid_event_name_checks() {
        assert!(is_valid_event_name("s3:ObjectCreated:*"));
        assert!(is_valid_event_name("s3:ObjectCreated:Put"));
        assert!(is_valid_event_name("s3:ObjectRemoved:Delete"));
        assert!(!is_valid_event_name("s3:InvalidEvent:Foo"));
        assert!(!is_valid_event_name("garbage"));
    }

    #[test]
    fn s3_event_to_record() {
        let event = S3Event {
            event_name: "s3:ObjectCreated:Put".to_string(),
            bucket: "my-bucket".to_string(),
            key: "my-key.txt".to_string(),
            size: 1024,
            etag: "\"abc123\"".to_string(),
            version_id: None,
            sequencer: "001".to_string(),
            user_identity: Some("AKIAEXAMPLE".to_string()),
            source_ip: Some("192.168.1.1".to_string()),
            request_id: Some("req-123".to_string()),
            timestamp: chrono::Utc::now(),
        };

        let record = event.to_record("hook1", "us-east-1");
        assert_eq!(record.event_version, "2.1");
        assert_eq!(record.event_source, "arca:s3");
        assert_eq!(record.event_name, "s3:ObjectCreated:Put");
        assert_eq!(record.s3.bucket.name, "my-bucket");
        assert_eq!(record.s3.object.key, "my-key.txt");
        assert_eq!(record.s3.object.size, 1024);
        assert_eq!(record.s3.configuration_id, "hook1");
        assert_eq!(record.region, "us-east-1");
        assert_eq!(record.user_identity.principal_id, "AKIAEXAMPLE");
        assert_eq!(record.request_parameters.source_ip_address, "192.168.1.1");
    }

    #[test]
    fn notification_configuration_all_configs() {
        let config = NotificationConfiguration {
            topic_configurations: vec![DestinationConfig {
                id: "t1".to_string(),
                destination_type: DestinationType::Topic,
                arn: "http://a.com".to_string(),
                events: vec!["s3:ObjectCreated:*".to_string()],
                filter: None,
            }],
            queue_configurations: vec![DestinationConfig {
                id: "q1".to_string(),
                destination_type: DestinationType::Queue,
                arn: "http://b.com".to_string(),
                events: vec!["s3:ObjectRemoved:*".to_string()],
                filter: None,
            }],
            cloud_function_configurations: vec![],
        };

        let all: Vec<_> = config.all_configs().collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "t1");
        assert_eq!(all[1].id, "q1");
    }

    #[test]
    fn notification_configuration_is_empty() {
        assert!(NotificationConfiguration::default().is_empty());

        let config = NotificationConfiguration {
            topic_configurations: vec![DestinationConfig {
                id: "t1".to_string(),
                destination_type: DestinationType::Topic,
                arn: "http://a.com".to_string(),
                events: vec!["s3:ObjectCreated:*".to_string()],
                filter: None,
            }],
            ..Default::default()
        };
        assert!(!config.is_empty());
    }
}
