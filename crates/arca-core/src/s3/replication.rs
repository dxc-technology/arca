//! S3 Replication Configuration types, XML parsing, and serialization.
//!
//! Mirrors the AWS S3 ReplicationConfiguration schema, with a few Arca-specific
//! extensions stored alongside the standard fields:
//!
//! - `destination.endpoint` — explicit S3 endpoint URL (Arca replicates to any
//!   S3-compatible endpoint, not just other Arca instances).
//! - `destination.credential_ref` — reference to a credential stored in
//!   `server_config` under a matching key. The actual access key and secret
//!   never travel over the wire.
//! - `destination.region` — region advertised in outbound SigV4 signatures.
//!
//! Storage: serde-JSON, living in the `bucket_config` table under the
//! `replication_configuration` key (same pattern as lifecycle/notification).

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;
use serde::{Deserialize, Serialize};

use crate::error::{write_xml_element, S3Error, S3ErrorCode};

// ── Data types ──────────────────────────────────────────────────────────────

/// A complete replication configuration for a bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationConfiguration {
    /// IAM role ARN in AWS; in Arca this field is accepted for compatibility
    /// but its value is not used (auth is via SigV4 credentials, not roles).
    #[serde(default)]
    pub role: String,
    pub rules: Vec<ReplicationRule>,
}

/// A single replication rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationRule {
    pub id: String,
    pub status: RuleStatus,
    /// Numeric priority used when multiple rules overlap. Lower number = higher priority.
    #[serde(default)]
    pub priority: i32,
    pub filter: ReplicationFilter,
    pub destination: Destination,
    /// Whether delete-marker creation should be replicated (matches S3's
    /// `DeleteMarkerReplication` element). Default is `Enabled`.
    #[serde(default = "default_delete_marker_replication")]
    pub delete_marker_replication: RuleStatus,
}

fn default_delete_marker_replication() -> RuleStatus {
    RuleStatus::Enabled
}

/// Whether a replication rule is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleStatus {
    Enabled,
    Disabled,
}

/// Filter that determines which objects a rule applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationFilter {
    /// Match objects with the given key prefix (empty string matches all).
    Prefix(String),
    /// Match objects that have this exact tag.
    Tag { key: String, value: String },
    /// Match objects satisfying all conditions (prefix + tags).
    And {
        prefix: Option<String>,
        tags: Vec<(String, String)>,
    },
    /// No filter, matches all objects.
    Empty,
}

impl ReplicationFilter {
    /// Returns the prefix this filter requires, if any.
    pub fn prefix(&self) -> Option<&str> {
        match self {
            ReplicationFilter::Prefix(p) if !p.is_empty() => Some(p),
            ReplicationFilter::And {
                prefix: Some(p), ..
            } if !p.is_empty() => Some(p),
            _ => None,
        }
    }

    /// Returns the tags this filter requires.
    pub fn tags(&self) -> Vec<(&str, &str)> {
        match self {
            ReplicationFilter::Tag { key, value } => vec![(key, value)],
            ReplicationFilter::And { tags, .. } => {
                tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
            }
            _ => vec![],
        }
    }

    /// True when the given key + tag set matches this filter.
    pub fn matches(&self, key: &str, tags: &[(String, String)]) -> bool {
        if let Some(p) = self.prefix() {
            if !key.starts_with(p) {
                return false;
            }
        }
        for (fk, fv) in self.tags() {
            let matched = tags.iter().any(|(tk, tv)| tk == fk && tv == fv);
            if !matched {
                return false;
            }
        }
        true
    }
}

/// Where replicated objects are written.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Destination {
    /// Destination bucket name (S3 surface spells this as an ARN; Arca accepts
    /// plain bucket names and ARNs, normalizing ARNs to the last segment).
    pub bucket: String,
    /// Full endpoint URL (e.g. `https://replica.example.com`). Arca-specific
    /// extension — AWS infers the endpoint from the destination region.
    pub endpoint: String,
    /// Region used in outbound SigV4 signing (default `us-east-1`).
    #[serde(default = "default_region")]
    pub region: String,
    /// Identifier of the credential in `server_config` holding the access key
    /// and secret used for outbound requests. Arca-specific extension.
    pub credential_ref: String,
    /// Optional storage class to request on the destination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
}

fn default_region() -> String {
    "us-east-1".to_string()
}

// ── XML parsing ─────────────────────────────────────────────────────────────

/// Parse an S3 ReplicationConfiguration XML request body.
pub fn parse_replication_configuration_xml(
    xml: &str,
) -> Result<ReplicationConfiguration, S3Error> {
    #[derive(Deserialize)]
    #[serde(rename = "ReplicationConfiguration")]
    struct XmlConfig {
        #[serde(rename = "Role", default)]
        role: Option<String>,
        #[serde(rename = "Rule", default)]
        rules: Vec<XmlRule>,
    }

    #[derive(Deserialize)]
    struct XmlRule {
        #[serde(rename = "ID", default)]
        id: Option<String>,
        #[serde(rename = "Status")]
        status: String,
        #[serde(rename = "Priority", default)]
        priority: Option<i32>,
        #[serde(rename = "Filter")]
        filter: Option<XmlFilter>,
        /// Legacy: pre-V2 replication used <Prefix> as a direct child of <Rule>.
        #[serde(rename = "Prefix")]
        legacy_prefix: Option<String>,
        #[serde(rename = "Destination")]
        destination: XmlDestination,
        #[serde(rename = "DeleteMarkerReplication")]
        delete_marker_replication: Option<XmlDeleteMarkerReplication>,
    }

    #[derive(Deserialize)]
    struct XmlFilter {
        #[serde(rename = "Prefix")]
        prefix: Option<String>,
        #[serde(rename = "Tag")]
        tag: Option<XmlTag>,
        #[serde(rename = "And")]
        and: Option<XmlAnd>,
    }

    #[derive(Deserialize)]
    struct XmlAnd {
        #[serde(rename = "Prefix")]
        prefix: Option<String>,
        #[serde(rename = "Tag", default)]
        tags: Vec<XmlTag>,
    }

    #[derive(Deserialize)]
    struct XmlTag {
        #[serde(rename = "Key")]
        key: String,
        #[serde(rename = "Value")]
        value: String,
    }

    #[derive(Deserialize)]
    struct XmlDestination {
        #[serde(rename = "Bucket")]
        bucket: String,
        #[serde(rename = "Endpoint")]
        endpoint: Option<String>,
        #[serde(rename = "Region")]
        region: Option<String>,
        #[serde(rename = "CredentialRef")]
        credential_ref: Option<String>,
        #[serde(rename = "StorageClass")]
        storage_class: Option<String>,
    }

    #[derive(Deserialize)]
    struct XmlDeleteMarkerReplication {
        #[serde(rename = "Status")]
        status: String,
    }

    let parsed: XmlConfig = quick_xml::de::from_str(xml).map_err(|e| {
        S3Error::with_message(
            S3ErrorCode::MalformedXML,
            format!("Invalid replication configuration XML: {e}"),
            "",
        )
    })?;

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for xml_rule in parsed.rules {
        let status = parse_status(&xml_rule.status)?;

        let filter = match (xml_rule.filter, xml_rule.legacy_prefix) {
            (Some(f), _) => {
                if let Some(and) = f.and {
                    let tags: Vec<(String, String)> =
                        and.tags.into_iter().map(|t| (t.key, t.value)).collect();
                    ReplicationFilter::And {
                        prefix: and.prefix,
                        tags,
                    }
                } else if let Some(tag) = f.tag {
                    ReplicationFilter::Tag {
                        key: tag.key,
                        value: tag.value,
                    }
                } else if let Some(prefix) = f.prefix {
                    if prefix.is_empty() {
                        ReplicationFilter::Empty
                    } else {
                        ReplicationFilter::Prefix(prefix)
                    }
                } else {
                    ReplicationFilter::Empty
                }
            }
            (None, Some(prefix)) if !prefix.is_empty() => ReplicationFilter::Prefix(prefix),
            _ => ReplicationFilter::Empty,
        };

        let destination = Destination {
            bucket: normalize_bucket(&xml_rule.destination.bucket),
            endpoint: xml_rule.destination.endpoint.unwrap_or_default(),
            region: xml_rule.destination.region.unwrap_or_else(default_region),
            credential_ref: xml_rule.destination.credential_ref.unwrap_or_default(),
            storage_class: xml_rule.destination.storage_class,
        };

        let delete_marker_replication = match xml_rule.delete_marker_replication {
            Some(d) => parse_status(&d.status)?,
            None => RuleStatus::Enabled,
        };

        rules.push(ReplicationRule {
            id: xml_rule.id.unwrap_or_default(),
            status,
            priority: xml_rule.priority.unwrap_or(0),
            filter,
            destination,
            delete_marker_replication,
        });
    }

    let config = ReplicationConfiguration {
        role: parsed.role.unwrap_or_default(),
        rules,
    };
    validate_configuration(&config)?;
    Ok(config)
}

fn parse_status(s: &str) -> Result<RuleStatus, S3Error> {
    match s {
        "Enabled" => Ok(RuleStatus::Enabled),
        "Disabled" => Ok(RuleStatus::Disabled),
        other => Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            format!("Invalid status: {other}. Must be 'Enabled' or 'Disabled'"),
            "",
        )),
    }
}

/// Accept both `arn:aws:s3:::bucket-name` and plain `bucket-name` forms.
fn normalize_bucket(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("arn:aws:s3:::") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

fn validate_configuration(config: &ReplicationConfiguration) -> Result<(), S3Error> {
    if config.rules.is_empty() {
        return Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "ReplicationConfiguration must contain at least one rule",
            "",
        ));
    }
    if config.rules.len() > 1000 {
        return Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "ReplicationConfiguration cannot have more than 1000 rules",
            "",
        ));
    }

    let mut seen_ids = std::collections::HashSet::new();
    for rule in &config.rules {
        if rule.destination.bucket.is_empty() {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!("Rule '{}' is missing a destination bucket", rule.id),
                "",
            ));
        }
        if rule.destination.endpoint.is_empty() {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!(
                    "Rule '{}' is missing destination endpoint; Arca replication requires an explicit <Endpoint> in the <Destination>",
                    rule.id
                ),
                "",
            ));
        }
        if rule.destination.credential_ref.is_empty() {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!(
                    "Rule '{}' is missing destination credential reference; provide <CredentialRef> in the <Destination>",
                    rule.id
                ),
                "",
            ));
        }
        if rule.id.len() > 255 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!(
                    "Rule ID '{}...' exceeds maximum length of 255 characters",
                    &rule.id[..50]
                ),
                "",
            ));
        }
        if !rule.id.is_empty() && !seen_ids.insert(rule.id.clone()) {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!("Duplicate rule ID: '{}'", rule.id),
                "",
            ));
        }
    }
    Ok(())
}

// ── XML serialization ───────────────────────────────────────────────────────

/// Serialize a ReplicationConfiguration to S3-compatible XML.
pub fn replication_configuration_to_xml(config: &ReplicationConfiguration) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("ReplicationConfiguration");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    write_xml_element(&mut writer, "Role", &config.role);

    for rule in &config.rules {
        writer
            .write_event(Event::Start(BytesStart::new("Rule")))
            .expect("write Rule start");

        write_xml_element(&mut writer, "ID", &rule.id);
        write_xml_element(&mut writer, "Status", status_str(rule.status));
        write_xml_element(&mut writer, "Priority", &rule.priority.to_string());

        write_filter_xml(&mut writer, &rule.filter);

        writer
            .write_event(Event::Start(BytesStart::new("Destination")))
            .expect("write Destination start");
        write_xml_element(&mut writer, "Bucket", &rule.destination.bucket);
        if !rule.destination.endpoint.is_empty() {
            write_xml_element(&mut writer, "Endpoint", &rule.destination.endpoint);
        }
        if !rule.destination.region.is_empty() {
            write_xml_element(&mut writer, "Region", &rule.destination.region);
        }
        if !rule.destination.credential_ref.is_empty() {
            write_xml_element(&mut writer, "CredentialRef", &rule.destination.credential_ref);
        }
        if let Some(ref sc) = rule.destination.storage_class {
            write_xml_element(&mut writer, "StorageClass", sc);
        }
        writer
            .write_event(Event::End(BytesEnd::new("Destination")))
            .expect("write Destination end");

        writer
            .write_event(Event::Start(BytesStart::new("DeleteMarkerReplication")))
            .expect("write DeleteMarkerReplication start");
        write_xml_element(&mut writer, "Status", status_str(rule.delete_marker_replication));
        writer
            .write_event(Event::End(BytesEnd::new("DeleteMarkerReplication")))
            .expect("write DeleteMarkerReplication end");

        writer
            .write_event(Event::End(BytesEnd::new("Rule")))
            .expect("write Rule end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("ReplicationConfiguration")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

fn status_str(status: RuleStatus) -> &'static str {
    match status {
        RuleStatus::Enabled => "Enabled",
        RuleStatus::Disabled => "Disabled",
    }
}

fn write_filter_xml(writer: &mut Writer<Vec<u8>>, filter: &ReplicationFilter) {
    writer
        .write_event(Event::Start(BytesStart::new("Filter")))
        .expect("write Filter start");

    match filter {
        ReplicationFilter::Empty => {
            write_xml_element(writer, "Prefix", "");
        }
        ReplicationFilter::Prefix(prefix) => {
            write_xml_element(writer, "Prefix", prefix);
        }
        ReplicationFilter::Tag { key, value } => {
            writer
                .write_event(Event::Start(BytesStart::new("Tag")))
                .expect("write Tag start");
            write_xml_element(writer, "Key", key);
            write_xml_element(writer, "Value", value);
            writer
                .write_event(Event::End(BytesEnd::new("Tag")))
                .expect("write Tag end");
        }
        ReplicationFilter::And { prefix, tags } => {
            writer
                .write_event(Event::Start(BytesStart::new("And")))
                .expect("write And start");
            if let Some(prefix) = prefix {
                write_xml_element(writer, "Prefix", prefix);
            }
            for (key, value) in tags {
                writer
                    .write_event(Event::Start(BytesStart::new("Tag")))
                    .expect("write Tag start");
                write_xml_element(writer, "Key", key);
                write_xml_element(writer, "Value", value);
                writer
                    .write_event(Event::End(BytesEnd::new("Tag")))
                    .expect("write Tag end");
            }
            writer
                .write_event(Event::End(BytesEnd::new("And")))
                .expect("write And end");
        }
    }

    writer
        .write_event(Event::End(BytesEnd::new("Filter")))
        .expect("write Filter end");
}

// ── Replication status on objects ───────────────────────────────────────────

/// Status carried in the `x-amz-replication-status` response header and in
/// the `objects.replication_status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationStatus {
    Pending,
    Completed,
    Failed,
    Replica,
}

impl ReplicationStatus {
    pub fn as_header(self) -> &'static str {
        match self {
            ReplicationStatus::Pending => "PENDING",
            ReplicationStatus::Completed => "COMPLETED",
            ReplicationStatus::Failed => "FAILED",
            ReplicationStatus::Replica => "REPLICA",
        }
    }

    pub fn parse(s: &str) -> Option<ReplicationStatus> {
        match s {
            "PENDING" => Some(ReplicationStatus::Pending),
            "COMPLETED" => Some(ReplicationStatus::Completed),
            "FAILED" => Some(ReplicationStatus::Failed),
            "REPLICA" => Some(ReplicationStatus::Replica),
            _ => None,
        }
    }
}

/// Header sent by the Arca replication worker on every outbound request, so
/// the receiving Arca can flag the object as REPLICA and the receiving
/// emit-logic can skip the replication journal (loop prevention).
pub const REPLICATION_SOURCE_HEADER: &str = "x-amz-arca-replication-source";

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn example_destination() -> Destination {
        Destination {
            bucket: "replica".to_string(),
            endpoint: "https://replica.example.com".to_string(),
            region: "us-east-1".to_string(),
            credential_ref: "replica-creds".to_string(),
            storage_class: None,
        }
    }

    #[test]
    fn parse_minimal_rule() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ReplicationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Role>arn:aws:iam::account:role/replication</Role>
  <Rule>
    <ID>rule-1</ID>
    <Status>Enabled</Status>
    <Priority>1</Priority>
    <Filter><Prefix>data/</Prefix></Filter>
    <Destination>
      <Bucket>arn:aws:s3:::replica</Bucket>
      <Endpoint>https://replica.example.com</Endpoint>
      <CredentialRef>replica-creds</CredentialRef>
    </Destination>
    <DeleteMarkerReplication><Status>Enabled</Status></DeleteMarkerReplication>
  </Rule>
</ReplicationConfiguration>"#;

        let config = parse_replication_configuration_xml(xml).unwrap();
        assert_eq!(config.rules.len(), 1);
        let rule = &config.rules[0];
        assert_eq!(rule.id, "rule-1");
        assert_eq!(rule.status, RuleStatus::Enabled);
        assert_eq!(rule.priority, 1);
        assert_eq!(rule.filter, ReplicationFilter::Prefix("data/".to_string()));
        assert_eq!(rule.destination.bucket, "replica");
        assert_eq!(rule.destination.endpoint, "https://replica.example.com");
        assert_eq!(rule.destination.credential_ref, "replica-creds");
        assert_eq!(rule.delete_marker_replication, RuleStatus::Enabled);
    }

    #[test]
    fn parse_legacy_prefix_form() {
        let xml = r#"<ReplicationConfiguration>
  <Role/>
  <Rule>
    <ID>legacy</ID>
    <Status>Enabled</Status>
    <Prefix>logs/</Prefix>
    <Destination>
      <Bucket>replica</Bucket>
      <Endpoint>https://replica.example.com</Endpoint>
      <CredentialRef>x</CredentialRef>
    </Destination>
  </Rule>
</ReplicationConfiguration>"#;

        let config = parse_replication_configuration_xml(xml).unwrap();
        assert_eq!(
            config.rules[0].filter,
            ReplicationFilter::Prefix("logs/".to_string())
        );
    }

    #[test]
    fn parse_and_filter() {
        let xml = r#"<ReplicationConfiguration>
  <Rule>
    <ID>r</ID>
    <Status>Enabled</Status>
    <Filter>
      <And>
        <Prefix>data/</Prefix>
        <Tag><Key>env</Key><Value>prod</Value></Tag>
      </And>
    </Filter>
    <Destination>
      <Bucket>replica</Bucket>
      <Endpoint>https://replica.example.com</Endpoint>
      <CredentialRef>x</CredentialRef>
    </Destination>
  </Rule>
</ReplicationConfiguration>"#;

        let config = parse_replication_configuration_xml(xml).unwrap();
        assert_eq!(
            config.rules[0].filter,
            ReplicationFilter::And {
                prefix: Some("data/".to_string()),
                tags: vec![("env".to_string(), "prod".to_string())],
            }
        );
    }

    #[test]
    fn reject_missing_endpoint() {
        let xml = r#"<ReplicationConfiguration>
  <Rule>
    <ID>r</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <Destination>
      <Bucket>replica</Bucket>
      <CredentialRef>x</CredentialRef>
    </Destination>
  </Rule>
</ReplicationConfiguration>"#;

        let err = parse_replication_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("endpoint"));
    }

    #[test]
    fn reject_missing_credential_ref() {
        let xml = r#"<ReplicationConfiguration>
  <Rule>
    <ID>r</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <Destination>
      <Bucket>replica</Bucket>
      <Endpoint>https://replica.example.com</Endpoint>
    </Destination>
  </Rule>
</ReplicationConfiguration>"#;

        let err = parse_replication_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("credential"));
    }

    #[test]
    fn reject_empty_configuration() {
        let xml = r#"<ReplicationConfiguration><Role/></ReplicationConfiguration>"#;
        let err = parse_replication_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn reject_duplicate_ids() {
        let xml = r#"<ReplicationConfiguration>
  <Role/>
  <Rule>
    <ID>dup</ID>
    <Status>Enabled</Status>
    <Filter><Prefix>a/</Prefix></Filter>
    <Destination>
      <Bucket>replica</Bucket>
      <Endpoint>https://replica.example.com</Endpoint>
      <CredentialRef>x</CredentialRef>
    </Destination>
  </Rule>
  <Rule>
    <ID>dup</ID>
    <Status>Enabled</Status>
    <Filter><Prefix>b/</Prefix></Filter>
    <Destination>
      <Bucket>replica</Bucket>
      <Endpoint>https://replica.example.com</Endpoint>
      <CredentialRef>x</CredentialRef>
    </Destination>
  </Rule>
</ReplicationConfiguration>"#;

        let err = parse_replication_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("Duplicate"));
    }

    #[test]
    fn xml_roundtrip() {
        let config = ReplicationConfiguration {
            role: "arn:aws:iam::account:role/r".to_string(),
            rules: vec![
                ReplicationRule {
                    id: "rule-a".to_string(),
                    status: RuleStatus::Enabled,
                    priority: 1,
                    filter: ReplicationFilter::Prefix("logs/".to_string()),
                    destination: example_destination(),
                    delete_marker_replication: RuleStatus::Enabled,
                },
                ReplicationRule {
                    id: "rule-b".to_string(),
                    status: RuleStatus::Disabled,
                    priority: 2,
                    filter: ReplicationFilter::And {
                        prefix: Some("data/".to_string()),
                        tags: vec![("env".to_string(), "prod".to_string())],
                    },
                    destination: Destination {
                        storage_class: Some("STANDARD_IA".to_string()),
                        ..example_destination()
                    },
                    delete_marker_replication: RuleStatus::Disabled,
                },
            ],
        };

        let xml = replication_configuration_to_xml(&config);
        let parsed = parse_replication_configuration_xml(&xml).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn json_roundtrip() {
        let config = ReplicationConfiguration {
            role: String::new(),
            rules: vec![ReplicationRule {
                id: "x".to_string(),
                status: RuleStatus::Enabled,
                priority: 0,
                filter: ReplicationFilter::Empty,
                destination: example_destination(),
                delete_marker_replication: RuleStatus::Enabled,
            }],
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ReplicationConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn filter_matches_prefix_and_tags() {
        let f = ReplicationFilter::And {
            prefix: Some("logs/".to_string()),
            tags: vec![("env".to_string(), "prod".to_string())],
        };
        assert!(f.matches(
            "logs/app.log",
            &[("env".to_string(), "prod".to_string())]
        ));
        assert!(!f.matches(
            "data/file",
            &[("env".to_string(), "prod".to_string())]
        ));
        assert!(!f.matches(
            "logs/app.log",
            &[("env".to_string(), "stage".to_string())]
        ));
        assert!(!f.matches("logs/app.log", &[]));
    }

    #[test]
    fn empty_filter_matches_everything() {
        assert!(ReplicationFilter::Empty.matches("any/key", &[]));
        assert!(ReplicationFilter::Prefix(String::new()).matches("any/key", &[]));
    }

    #[test]
    fn replication_status_header_roundtrip() {
        assert_eq!(
            ReplicationStatus::parse(ReplicationStatus::Pending.as_header()),
            Some(ReplicationStatus::Pending)
        );
        assert_eq!(
            ReplicationStatus::parse(ReplicationStatus::Completed.as_header()),
            Some(ReplicationStatus::Completed)
        );
        assert_eq!(
            ReplicationStatus::parse(ReplicationStatus::Failed.as_header()),
            Some(ReplicationStatus::Failed)
        );
        assert_eq!(
            ReplicationStatus::parse(ReplicationStatus::Replica.as_header()),
            Some(ReplicationStatus::Replica)
        );
        assert_eq!(ReplicationStatus::parse("nonsense"), None);
    }
}
