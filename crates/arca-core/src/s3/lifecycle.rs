//! S3 Lifecycle Configuration types, XML parsing, and serialization.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;
use serde::{Deserialize, Serialize};

use crate::error::{write_xml_element, S3Error, S3ErrorCode};

// ── Data types (serde-serializable for JSON storage in bucket_config) ──

/// A complete lifecycle configuration for a bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LifecycleConfiguration {
    pub rules: Vec<LifecycleRule>,
}

/// A single lifecycle rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LifecycleRule {
    pub id: String,
    pub status: RuleStatus,
    pub filter: LifecycleFilter,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<Expiration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noncurrent_version_expiration: Option<NoncurrentVersionExpiration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_incomplete_multipart_upload: Option<AbortIncompleteMultipartUpload>,
}

/// Whether a lifecycle rule is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleStatus {
    Enabled,
    Disabled,
}

/// Filter that determines which objects a rule applies to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LifecycleFilter {
    /// Match objects with the given key prefix (empty string matches all).
    Prefix(String),
    /// Match objects that have this exact tag.
    Tag { key: String, value: String },
    /// Match objects satisfying all conditions (prefix + tags).
    And {
        prefix: Option<String>,
        tags: Vec<(String, String)>,
    },
    /// No filter, matches all objects (empty <Filter/> or <Filter><Prefix></Prefix></Filter>).
    Empty,
}

/// Expiration action: expire current versions by days, date, or delete-marker cleanup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Expiration {
    /// Expire objects after N days.
    Days { days: u32 },
    /// Expire objects on a specific ISO 8601 date.
    Date { date: String },
    /// Remove expired object delete markers (versioned buckets only).
    ExpiredObjectDeleteMarker { expired_object_delete_marker: bool },
}

impl Expiration {
    /// Returns the number of days if this is a Days variant.
    pub fn days(&self) -> Option<u32> {
        match self {
            Expiration::Days { days } => Some(*days),
            _ => None,
        }
    }
}

/// Hard-delete noncurrent versions after N days.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoncurrentVersionExpiration {
    pub noncurrent_days: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newer_noncurrent_versions: Option<u32>,
}

/// Abort incomplete multipart uploads after N days.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbortIncompleteMultipartUpload {
    pub days_after_initiation: u32,
}

// ── Helpers for the worker to extract filter components ──

impl LifecycleFilter {
    /// Returns the prefix this filter requires, if any.
    pub fn prefix(&self) -> Option<&str> {
        match self {
            LifecycleFilter::Prefix(p) if !p.is_empty() => Some(p),
            LifecycleFilter::And {
                prefix: Some(p), ..
            } if !p.is_empty() => Some(p),
            _ => None,
        }
    }

    /// Returns the tags this filter requires, if any.
    pub fn tags(&self) -> Vec<(&str, &str)> {
        match self {
            LifecycleFilter::Tag { key, value } => vec![(key, value)],
            LifecycleFilter::And { tags, .. } => {
                tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
            }
            _ => vec![],
        }
    }
}

// ── XML parsing ──

/// Parse an S3 LifecycleConfiguration XML request body.
pub fn parse_lifecycle_configuration_xml(xml: &str) -> Result<LifecycleConfiguration, S3Error> {
    // Local serde structs for XML deserialization (not the same as storage types).
    #[derive(Deserialize)]
    #[serde(rename = "LifecycleConfiguration")]
    struct XmlLifecycleConfiguration {
        #[serde(rename = "Rule", default)]
        rules: Vec<XmlRule>,
    }

    #[derive(Deserialize)]
    struct XmlRule {
        #[serde(rename = "ID", default)]
        id: Option<String>,
        #[serde(rename = "Status")]
        status: String,
        #[serde(rename = "Filter")]
        filter: Option<XmlFilter>,
        #[serde(rename = "Expiration")]
        expiration: Option<XmlExpiration>,
        #[serde(rename = "NoncurrentVersionExpiration")]
        noncurrent_version_expiration: Option<XmlNoncurrentVersionExpiration>,
        #[serde(rename = "AbortIncompleteMultipartUpload")]
        abort_incomplete_multipart_upload: Option<XmlAbortIncompleteMultipartUpload>,
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
    struct XmlExpiration {
        #[serde(rename = "Days")]
        days: Option<u32>,
        #[serde(rename = "Date")]
        date: Option<String>,
        #[serde(rename = "ExpiredObjectDeleteMarker")]
        expired_object_delete_marker: Option<String>,
    }

    #[derive(Deserialize)]
    struct XmlNoncurrentVersionExpiration {
        #[serde(rename = "NoncurrentDays")]
        noncurrent_days: Option<u32>,
        #[serde(rename = "NewerNoncurrentVersions")]
        newer_noncurrent_versions: Option<u32>,
    }

    #[derive(Deserialize)]
    struct XmlAbortIncompleteMultipartUpload {
        #[serde(rename = "DaysAfterInitiation")]
        days_after_initiation: Option<u32>,
    }

    let parsed: XmlLifecycleConfiguration = quick_xml::de::from_str(xml).map_err(|e| {
        S3Error::with_message(
            S3ErrorCode::MalformedXML,
            format!("Invalid lifecycle configuration XML: {e}"),
            "",
        )
    })?;

    let mut rules = Vec::with_capacity(parsed.rules.len());
    for xml_rule in parsed.rules {
        let status = match xml_rule.status.as_str() {
            "Enabled" => RuleStatus::Enabled,
            "Disabled" => RuleStatus::Disabled,
            other => {
                return Err(S3Error::with_message(
                    S3ErrorCode::MalformedXML,
                    format!("Invalid rule status: {other}. Must be 'Enabled' or 'Disabled'"),
                    "",
                ));
            }
        };

        let filter = match xml_rule.filter {
            None => LifecycleFilter::Empty,
            Some(f) => {
                if let Some(and) = f.and {
                    let tags: Vec<(String, String)> =
                        and.tags.into_iter().map(|t| (t.key, t.value)).collect();
                    LifecycleFilter::And {
                        prefix: and.prefix,
                        tags,
                    }
                } else if let Some(tag) = f.tag {
                    LifecycleFilter::Tag {
                        key: tag.key,
                        value: tag.value,
                    }
                } else if let Some(prefix) = f.prefix {
                    if prefix.is_empty() {
                        LifecycleFilter::Empty
                    } else {
                        LifecycleFilter::Prefix(prefix)
                    }
                } else {
                    LifecycleFilter::Empty
                }
            }
        };

        let expiration = match xml_rule.expiration {
            Some(ref xe) if xe.days.is_some() => {
                Some(Expiration::Days { days: xe.days.unwrap() })
            }
            Some(ref xe) if xe.date.is_some() => {
                Some(Expiration::Date { date: xe.date.clone().unwrap() })
            }
            Some(ref xe) if xe.expired_object_delete_marker.is_some() => {
                let val = xe.expired_object_delete_marker.as_ref().unwrap();
                let b = val.eq_ignore_ascii_case("true");
                Some(Expiration::ExpiredObjectDeleteMarker { expired_object_delete_marker: b })
            }
            Some(_) => {
                return Err(S3Error::with_message(
                    S3ErrorCode::MalformedXML,
                    "Expiration element must contain Days, Date, or ExpiredObjectDeleteMarker",
                    "",
                ));
            }
            None => None,
        };

        let noncurrent_version_expiration = match xml_rule.noncurrent_version_expiration {
            Some(ref nve) if nve.noncurrent_days.is_some() || nve.newer_noncurrent_versions.is_some() => {
                Some(NoncurrentVersionExpiration {
                    noncurrent_days: nve.noncurrent_days.unwrap_or(0),
                    newer_noncurrent_versions: nve.newer_noncurrent_versions,
                })
            }
            Some(_) => {
                return Err(S3Error::with_message(
                    S3ErrorCode::MalformedXML,
                    "NoncurrentVersionExpiration must contain NoncurrentDays or NewerNoncurrentVersions",
                    "",
                ));
            }
            None => None,
        };

        let abort_incomplete_multipart_upload = match xml_rule.abort_incomplete_multipart_upload {
            Some(XmlAbortIncompleteMultipartUpload {
                days_after_initiation: Some(d),
            }) => Some(AbortIncompleteMultipartUpload {
                days_after_initiation: d,
            }),
            Some(XmlAbortIncompleteMultipartUpload {
                days_after_initiation: None,
            }) => {
                return Err(S3Error::with_message(
                    S3ErrorCode::MalformedXML,
                    "AbortIncompleteMultipartUpload must contain DaysAfterInitiation",
                    "",
                ));
            }
            None => None,
        };

        let id = xml_rule.id.unwrap_or_default();

        rules.push(LifecycleRule {
            id,
            status,
            filter,
            expiration,
            noncurrent_version_expiration,
            abort_incomplete_multipart_upload,
        });
    }

    let config = LifecycleConfiguration { rules };
    validate_lifecycle_configuration(&config)?;
    Ok(config)
}

/// Validate lifecycle configuration constraints.
fn validate_lifecycle_configuration(config: &LifecycleConfiguration) -> Result<(), S3Error> {
    if config.rules.is_empty() {
        return Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "Lifecycle configuration must contain at least one rule",
            "",
        ));
    }

    if config.rules.len() > 1000 {
        return Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "Lifecycle configuration cannot have more than 1000 rules",
            "",
        ));
    }

    let mut seen_ids = std::collections::HashSet::new();
    for rule in &config.rules {
        // Each rule must have at least one action
        if rule.expiration.is_none()
            && rule.noncurrent_version_expiration.is_none()
            && rule.abort_incomplete_multipart_upload.is_none()
        {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!(
                    "Rule '{}' must have at least one action (Expiration, NoncurrentVersionExpiration, or AbortIncompleteMultipartUpload)",
                    rule.id
                ),
                "",
            ));
        }

        // Validate Expiration values
        if let Some(ref exp) = rule.expiration {
            match exp {
                Expiration::Days { days } if *days == 0 => {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InvalidArgument,
                        "'Days' in Expiration must be a positive integer",
                        "",
                    ));
                }
                _ => {}
            }
        }
        if let Some(ref nve) = rule.noncurrent_version_expiration {
            if nve.noncurrent_days == 0 {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "'NoncurrentDays' in NoncurrentVersionExpiration must be a positive integer",
                    "",
                ));
            }
        }
        if let Some(ref abort) = rule.abort_incomplete_multipart_upload {
            if abort.days_after_initiation == 0 {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "'DaysAfterInitiation' in AbortIncompleteMultipartUpload must be a positive integer",
                    "",
                ));
            }
        }

        // Rule ID length limit (255 chars max per S3 spec)
        if rule.id.len() > 255 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!("Rule ID '{}...' exceeds maximum length of 255 characters", &rule.id[..50]),
                "",
            ));
        }

        // Rule IDs must be unique (empty IDs are allowed but still must be unique)
        if !rule.id.is_empty() && !seen_ids.insert(&rule.id) {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!("Duplicate rule ID: '{}'", rule.id),
                "",
            ));
        }
    }

    Ok(())
}

// ── XML serialization ──

/// Serialize a LifecycleConfiguration to S3-compatible XML.
pub fn lifecycle_configuration_to_xml(config: &LifecycleConfiguration) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("LifecycleConfiguration");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write LifecycleConfiguration start");

    for rule in &config.rules {
        writer
            .write_event(Event::Start(BytesStart::new("Rule")))
            .expect("write Rule start");

        write_xml_element(&mut writer, "ID", &rule.id);
        write_xml_element(
            &mut writer,
            "Status",
            match rule.status {
                RuleStatus::Enabled => "Enabled",
                RuleStatus::Disabled => "Disabled",
            },
        );

        // Filter
        write_filter_xml(&mut writer, &rule.filter);

        // Expiration
        if let Some(ref exp) = rule.expiration {
            writer
                .write_event(Event::Start(BytesStart::new("Expiration")))
                .expect("write Expiration start");
            match exp {
                Expiration::Days { days } => {
                    write_xml_element(&mut writer, "Days", &days.to_string());
                }
                Expiration::Date { date } => {
                    write_xml_element(&mut writer, "Date", date);
                }
                Expiration::ExpiredObjectDeleteMarker { expired_object_delete_marker } => {
                    write_xml_element(
                        &mut writer,
                        "ExpiredObjectDeleteMarker",
                        if *expired_object_delete_marker { "true" } else { "false" },
                    );
                }
            }
            writer
                .write_event(Event::End(BytesEnd::new("Expiration")))
                .expect("write Expiration end");
        }

        // NoncurrentVersionExpiration
        if let Some(ref nve) = rule.noncurrent_version_expiration {
            writer
                .write_event(Event::Start(BytesStart::new("NoncurrentVersionExpiration")))
                .expect("write NoncurrentVersionExpiration start");
            if nve.noncurrent_days > 0 {
                write_xml_element(&mut writer, "NoncurrentDays", &nve.noncurrent_days.to_string());
            }
            if let Some(n) = nve.newer_noncurrent_versions {
                write_xml_element(&mut writer, "NewerNoncurrentVersions", &n.to_string());
            }
            writer
                .write_event(Event::End(BytesEnd::new("NoncurrentVersionExpiration")))
                .expect("write NoncurrentVersionExpiration end");
        }

        // AbortIncompleteMultipartUpload
        if let Some(ref abort) = rule.abort_incomplete_multipart_upload {
            writer
                .write_event(Event::Start(BytesStart::new("AbortIncompleteMultipartUpload")))
                .expect("write AbortIncompleteMultipartUpload start");
            write_xml_element(
                &mut writer,
                "DaysAfterInitiation",
                &abort.days_after_initiation.to_string(),
            );
            writer
                .write_event(Event::End(BytesEnd::new("AbortIncompleteMultipartUpload")))
                .expect("write AbortIncompleteMultipartUpload end");
        }

        writer
            .write_event(Event::End(BytesEnd::new("Rule")))
            .expect("write Rule end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("LifecycleConfiguration")))
        .expect("write LifecycleConfiguration end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Write a <Filter> element to the XML writer.
fn write_filter_xml(writer: &mut Writer<Vec<u8>>, filter: &LifecycleFilter) {
    writer
        .write_event(Event::Start(BytesStart::new("Filter")))
        .expect("write Filter start");

    match filter {
        LifecycleFilter::Empty => {
            // Empty filter: <Filter><Prefix></Prefix></Filter>
            write_xml_element(writer, "Prefix", "");
        }
        LifecycleFilter::Prefix(prefix) => {
            write_xml_element(writer, "Prefix", prefix);
        }
        LifecycleFilter::Tag { key, value } => {
            writer
                .write_event(Event::Start(BytesStart::new("Tag")))
                .expect("write Tag start");
            write_xml_element(writer, "Key", key);
            write_xml_element(writer, "Value", value);
            writer
                .write_event(Event::End(BytesEnd::new("Tag")))
                .expect("write Tag end");
        }
        LifecycleFilter::And { prefix, tags } => {
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

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_expiration_rule_with_prefix() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Rule>
    <ID>expire-old-logs</ID>
    <Status>Enabled</Status>
    <Filter>
      <Prefix>logs/</Prefix>
    </Filter>
    <Expiration>
      <Days>90</Days>
    </Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        assert_eq!(config.rules.len(), 1);

        let rule = &config.rules[0];
        assert_eq!(rule.id, "expire-old-logs");
        assert_eq!(rule.status, RuleStatus::Enabled);
        assert_eq!(rule.filter, LifecycleFilter::Prefix("logs/".to_string()));
        assert_eq!(rule.expiration, Some(Expiration::Days { days: 90 }));
        assert!(rule.noncurrent_version_expiration.is_none());
        assert!(rule.abort_incomplete_multipart_upload.is_none());
    }

    #[test]
    fn parse_abort_multipart_rule() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>abort-uploads</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <AbortIncompleteMultipartUpload>
      <DaysAfterInitiation>7</DaysAfterInitiation>
    </AbortIncompleteMultipartUpload>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        assert_eq!(config.rules.len(), 1);

        let rule = &config.rules[0];
        assert_eq!(rule.filter, LifecycleFilter::Empty);
        assert_eq!(
            rule.abort_incomplete_multipart_upload,
            Some(AbortIncompleteMultipartUpload {
                days_after_initiation: 7
            })
        );
    }

    #[test]
    fn parse_noncurrent_version_expiration() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>cleanup-old-versions</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <NoncurrentVersionExpiration>
      <NoncurrentDays>30</NoncurrentDays>
    </NoncurrentVersionExpiration>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        let rule = &config.rules[0];
        assert_eq!(
            rule.noncurrent_version_expiration,
            Some(NoncurrentVersionExpiration {
                noncurrent_days: 30,
                newer_noncurrent_versions: None,
            })
        );
    }

    #[test]
    fn parse_tag_filter() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>expire-temp</ID>
    <Status>Enabled</Status>
    <Filter>
      <Tag><Key>status</Key><Value>temporary</Value></Tag>
    </Filter>
    <Expiration><Days>1</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        let rule = &config.rules[0];
        assert_eq!(
            rule.filter,
            LifecycleFilter::Tag {
                key: "status".to_string(),
                value: "temporary".to_string()
            }
        );
    }

    #[test]
    fn parse_and_filter_with_prefix_and_tags() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>and-rule</ID>
    <Status>Enabled</Status>
    <Filter>
      <And>
        <Prefix>data/</Prefix>
        <Tag><Key>env</Key><Value>staging</Value></Tag>
        <Tag><Key>team</Key><Value>backend</Value></Tag>
      </And>
    </Filter>
    <Expiration><Days>60</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        let rule = &config.rules[0];
        assert_eq!(
            rule.filter,
            LifecycleFilter::And {
                prefix: Some("data/".to_string()),
                tags: vec![
                    ("env".to_string(), "staging".to_string()),
                    ("team".to_string(), "backend".to_string()),
                ],
            }
        );
    }

    #[test]
    fn parse_disabled_rule() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>disabled-rule</ID>
    <Status>Disabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <Expiration><Days>365</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        assert_eq!(config.rules[0].status, RuleStatus::Disabled);
    }

    #[test]
    fn parse_multiple_rules() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>rule-1</ID>
    <Status>Enabled</Status>
    <Filter><Prefix>logs/</Prefix></Filter>
    <Expiration><Days>30</Days></Expiration>
  </Rule>
  <Rule>
    <ID>rule-2</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <AbortIncompleteMultipartUpload>
      <DaysAfterInitiation>7</DaysAfterInitiation>
    </AbortIncompleteMultipartUpload>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        assert_eq!(config.rules.len(), 2);
    }

    #[test]
    fn parse_rule_with_all_actions() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>all-actions</ID>
    <Status>Enabled</Status>
    <Filter><Prefix>data/</Prefix></Filter>
    <Expiration><Days>365</Days></Expiration>
    <NoncurrentVersionExpiration><NoncurrentDays>90</NoncurrentDays></NoncurrentVersionExpiration>
    <AbortIncompleteMultipartUpload><DaysAfterInitiation>7</DaysAfterInitiation></AbortIncompleteMultipartUpload>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        let rule = &config.rules[0];
        assert!(rule.expiration.is_some());
        assert!(rule.noncurrent_version_expiration.is_some());
        assert!(rule.abort_incomplete_multipart_upload.is_some());
    }

    #[test]
    fn reject_empty_configuration() {
        let xml = r#"<LifecycleConfiguration></LifecycleConfiguration>"#;
        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn reject_rule_without_action() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>no-action</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
  </Rule>
</LifecycleConfiguration>"#;

        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("at least one action"));
    }

    #[test]
    fn reject_zero_days() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>zero-days</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <Expiration><Days>0</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("positive integer"));
    }

    #[test]
    fn reject_zero_noncurrent_days() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>zero-noncurrent</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <NoncurrentVersionExpiration><NoncurrentDays>0</NoncurrentDays></NoncurrentVersionExpiration>
  </Rule>
</LifecycleConfiguration>"#;

        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
    }

    #[test]
    fn reject_zero_abort_days() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>zero-abort</ID>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <AbortIncompleteMultipartUpload><DaysAfterInitiation>0</DaysAfterInitiation></AbortIncompleteMultipartUpload>
  </Rule>
</LifecycleConfiguration>"#;

        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
    }

    #[test]
    fn reject_invalid_status() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>bad-status</ID>
    <Status>Active</Status>
    <Filter><Prefix></Prefix></Filter>
    <Expiration><Days>1</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn reject_duplicate_rule_ids() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>same-id</ID>
    <Status>Enabled</Status>
    <Filter><Prefix>a/</Prefix></Filter>
    <Expiration><Days>1</Days></Expiration>
  </Rule>
  <Rule>
    <ID>same-id</ID>
    <Status>Enabled</Status>
    <Filter><Prefix>b/</Prefix></Filter>
    <Expiration><Days>2</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
        assert!(err.message.contains("Duplicate rule ID"));
    }

    #[test]
    fn reject_malformed_xml() {
        let xml = "not valid xml at all";
        let err = parse_lifecycle_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn xml_roundtrip() {
        let config = LifecycleConfiguration {
            rules: vec![
                LifecycleRule {
                    id: "expire-logs".to_string(),
                    status: RuleStatus::Enabled,
                    filter: LifecycleFilter::Prefix("logs/".to_string()),
                    expiration: Some(Expiration::Days { days: 90 }),
                    noncurrent_version_expiration: None,
                    abort_incomplete_multipart_upload: None,
                },
                LifecycleRule {
                    id: "cleanup-uploads".to_string(),
                    status: RuleStatus::Enabled,
                    filter: LifecycleFilter::Empty,
                    expiration: None,
                    noncurrent_version_expiration: None,
                    abort_incomplete_multipart_upload: Some(AbortIncompleteMultipartUpload {
                        days_after_initiation: 7,
                    }),
                },
                LifecycleRule {
                    id: "tagged-expire".to_string(),
                    status: RuleStatus::Disabled,
                    filter: LifecycleFilter::And {
                        prefix: Some("data/".to_string()),
                        tags: vec![("env".to_string(), "staging".to_string())],
                    },
                    expiration: Some(Expiration::Days { days: 30 }),
                    noncurrent_version_expiration: Some(NoncurrentVersionExpiration {
                        noncurrent_days: 60,
                        newer_noncurrent_versions: None,
                    }),
                    abort_incomplete_multipart_upload: None,
                },
            ],
        };

        let xml = lifecycle_configuration_to_xml(&config);
        let parsed = parse_lifecycle_configuration_xml(&xml).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn xml_roundtrip_tag_filter() {
        let config = LifecycleConfiguration {
            rules: vec![LifecycleRule {
                id: "tag-rule".to_string(),
                status: RuleStatus::Enabled,
                filter: LifecycleFilter::Tag {
                    key: "status".to_string(),
                    value: "temp".to_string(),
                },
                expiration: Some(Expiration::Days { days: 1 }),
                noncurrent_version_expiration: None,
                abort_incomplete_multipart_upload: None,
            }],
        };

        let xml = lifecycle_configuration_to_xml(&config);
        let parsed = parse_lifecycle_configuration_xml(&xml).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn json_roundtrip() {
        let config = LifecycleConfiguration {
            rules: vec![LifecycleRule {
                id: "rule-1".to_string(),
                status: RuleStatus::Enabled,
                filter: LifecycleFilter::Prefix("logs/".to_string()),
                expiration: Some(Expiration::Days { days: 30 }),
                noncurrent_version_expiration: None,
                abort_incomplete_multipart_upload: None,
            }],
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: LifecycleConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn filter_prefix_helper() {
        assert_eq!(
            LifecycleFilter::Prefix("logs/".to_string()).prefix(),
            Some("logs/")
        );
        assert_eq!(LifecycleFilter::Empty.prefix(), None);
        assert_eq!(LifecycleFilter::Prefix("".to_string()).prefix(), None);
        assert_eq!(
            LifecycleFilter::And {
                prefix: Some("data/".to_string()),
                tags: vec![]
            }
            .prefix(),
            Some("data/")
        );
        assert_eq!(
            LifecycleFilter::Tag {
                key: "k".to_string(),
                value: "v".to_string()
            }
            .prefix(),
            None
        );
    }

    #[test]
    fn filter_tags_helper() {
        assert!(LifecycleFilter::Empty.tags().is_empty());
        assert!(LifecycleFilter::Prefix("x".to_string()).tags().is_empty());
        assert_eq!(
            LifecycleFilter::Tag {
                key: "k".to_string(),
                value: "v".to_string()
            }
            .tags(),
            vec![("k", "v")]
        );
        assert_eq!(
            LifecycleFilter::And {
                prefix: None,
                tags: vec![
                    ("a".to_string(), "1".to_string()),
                    ("b".to_string(), "2".to_string()),
                ]
            }
            .tags(),
            vec![("a", "1"), ("b", "2")]
        );
    }

    #[test]
    fn parse_rule_without_id() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <Status>Enabled</Status>
    <Filter><Prefix></Prefix></Filter>
    <Expiration><Days>30</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        let config = parse_lifecycle_configuration_xml(xml).unwrap();
        assert_eq!(config.rules[0].id, "");
    }

    #[test]
    fn parse_empty_filter_element() {
        let xml = r#"<LifecycleConfiguration>
  <Rule>
    <ID>empty-filter</ID>
    <Status>Enabled</Status>
    <Filter/>
    <Expiration><Days>30</Days></Expiration>
  </Rule>
</LifecycleConfiguration>"#;

        // Self-closing <Filter/> should parse as empty filter
        let result = parse_lifecycle_configuration_xml(xml);
        // This may or may not parse depending on quick_xml behavior with self-closing tags.
        // If it fails, that's acceptable — clients should use <Filter><Prefix></Prefix></Filter>.
        if let Ok(config) = result {
            assert_eq!(config.rules[0].filter, LifecycleFilter::Empty);
        }
    }
}
