//! S3 XML request/response types.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::error::write_xml_element;
use crate::types::{
    BucketInfo, ListBucketResultParams, ListBucketV1ResultParams, ListEntry,
    MultipartUploadRecord, PartRecord,
};


// -- Multipart upload XML types --

/// Parsed body of a `CompleteMultipartUpload` request.
#[derive(Debug, serde::Deserialize)]
#[serde(rename = "CompleteMultipartUpload")]
pub struct CompleteMultipartUploadBody {
    #[serde(rename = "Part", default)]
    pub parts: Vec<CompletePart>,
}

/// A single part reference in a `CompleteMultipartUpload` request.
#[derive(Debug, serde::Deserialize)]
pub struct CompletePart {
    #[serde(rename = "PartNumber")]
    pub part_number: u32,
    #[serde(rename = "ETag")]
    pub etag: String,
}

/// Parses a `CompleteMultipartUpload` XML request body.
pub fn parse_complete_multipart_upload(
    xml: &str,
) -> Result<CompleteMultipartUploadBody, quick_xml::DeError> {
    quick_xml::de::from_str(xml)
}

// -- DeleteObjects XML types --

/// Parsed body of a `DeleteObjects` request (`POST /{bucket}?delete`).
#[derive(Debug, serde::Deserialize)]
#[serde(rename = "Delete")]
pub struct DeleteObjectsBody {
    #[serde(rename = "Quiet", default)]
    pub quiet: bool,
    #[serde(rename = "Object")]
    pub objects: Vec<DeleteObject>,
}

/// A single object key in a `DeleteObjects` request.
#[derive(Debug, serde::Deserialize)]
pub struct DeleteObject {
    #[serde(rename = "Key")]
    pub key: String,
    /// Optional version ID for version-specific deletion.
    #[serde(rename = "VersionId", default)]
    pub version_id: Option<String>,
    /// Optional ETag for conditional delete (If-Match semantics per-key).
    #[serde(rename = "ETag", default)]
    pub etag: Option<String>,
    /// Optional last-modified-time conditional.
    #[serde(rename = "LastModifiedTime", default)]
    pub last_modified_time: Option<String>,
    /// Optional size conditional.
    #[serde(rename = "Size", default)]
    pub size: Option<String>,
}

/// Parses a `DeleteObjects` XML request body.
///
/// Uses the event-based API instead of serde because quick_xml's serde
/// deserializer trims whitespace from text nodes, which corrupts keys
/// that are whitespace-only (e.g. `" "`).
pub fn parse_delete_objects(xml: &str) -> Result<DeleteObjectsBody, quick_xml::DeError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    // Ensure whitespace in text content is preserved.
    reader.config_mut().trim_text(false);

    let mut quiet = false;
    let mut objects = Vec::new();
    let mut current_key: Option<String> = None;
    let mut current_version_id: Option<String> = None;
    let mut current_etag: Option<String> = None;
    let mut current_last_modified_time: Option<String> = None;
    let mut current_if_match_size: Option<String> = None;
    let mut inside_tag: Option<String> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                inside_tag = Some(name);
            }
            Ok(Event::Text(e)) => {
                if let Some(ref tag) = inside_tag {
                    let text = e.unescape().map_err(|e| quick_xml::DeError::InvalidXml(e.into()))?.to_string();
                    match tag.as_str() {
                        "Key" => current_key = Some(text),
                        "VersionId" => current_version_id = Some(text),
                        "ETag" => current_etag = Some(text),
                        "LastModifiedTime" => current_last_modified_time = Some(text),
                        "Size" => current_if_match_size = Some(text),
                        "Quiet" => quiet = text.trim() == "true",
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Object" {
                    if let Some(key) = current_key.take() {
                        objects.push(DeleteObject {
                            key,
                            version_id: current_version_id.take(),
                            etag: current_etag.take(),
                            last_modified_time: current_last_modified_time.take(),
                            size: current_if_match_size.take(),
                        });
                    }
                }
                inside_tag = None;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(quick_xml::DeError::InvalidXml(e.into())),
            _ => {}
        }
        buf.clear();
    }

    Ok(DeleteObjectsBody { quiet, objects })
}

/// Builds the XML response for the `ListAllMyBucketsResult` (ListBuckets).
///
/// Produces XML like:
/// ```xml
/// <?xml version="1.0" encoding="UTF-8"?>
/// <ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
///   <Owner>
///     <ID>arca</ID>
///     <DisplayName>arca</DisplayName>
///   </Owner>
///   <Buckets>
///     <Bucket>
///       <Name>my-bucket</Name>
///       <CreationDate>2024-01-01T00:00:00.000Z</CreationDate>
///     </Bucket>
///   </Buckets>
/// </ListAllMyBucketsResult>
/// ```
pub fn list_all_my_buckets_result(buckets: &[BucketInfo], owner: &str) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    // <ListAllMyBucketsResult xmlns="...">
    let mut root = BytesStart::new("ListAllMyBucketsResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    // <Owner>
    writer
        .write_event(Event::Start(BytesStart::new("Owner")))
        .expect("write Owner start");
    write_xml_element(&mut writer, "ID", owner);
    write_xml_element(&mut writer, "DisplayName", owner);
    writer
        .write_event(Event::End(BytesEnd::new("Owner")))
        .expect("write Owner end");

    // <Buckets>
    writer
        .write_event(Event::Start(BytesStart::new("Buckets")))
        .expect("write Buckets start");

    for bucket in buckets {
        writer
            .write_event(Event::Start(BytesStart::new("Bucket")))
            .expect("write Bucket start");

        write_xml_element(&mut writer, "Name", &bucket.name);

        // S3 uses ISO 8601 with milliseconds
        let date = bucket.created_at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        write_xml_element(&mut writer, "CreationDate", &date);

        writer
            .write_event(Event::End(BytesEnd::new("Bucket")))
            .expect("write Bucket end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("Buckets")))
        .expect("write Buckets end");

    // </ListAllMyBucketsResult>
    writer
        .write_event(Event::End(BytesEnd::new("ListAllMyBucketsResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `CopyObjectResult`.
///
/// Produces XML like:
/// ```xml
/// <?xml version="1.0" encoding="UTF-8"?>
/// <CopyObjectResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
///   <ETag>"etag"</ETag>
///   <LastModified>2024-01-01T00:00:00.000Z</LastModified>
/// </CopyObjectResult>
/// ```
pub fn copy_object_result(etag: &str, last_modified: &chrono::DateTime<chrono::Utc>) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("CopyObjectResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    let quoted_etag = format!("\"{}\"", etag);
    write_xml_element(&mut writer, "ETag", &quoted_etag);

    let date = last_modified.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    write_xml_element(&mut writer, "LastModified", &date);

    writer
        .write_event(Event::End(BytesEnd::new("CopyObjectResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `ListBucketResult` (ListObjectsV2).
pub fn list_bucket_result(params: &ListBucketResultParams) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("ListBucketResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    write_xml_element(&mut writer, "Name", params.name);

    if let Some(prefix) = params.prefix {
        write_xml_element(&mut writer, "Prefix", prefix);
    } else {
        write_xml_element(&mut writer, "Prefix", "");
    }

    write_xml_element(&mut writer, "MaxKeys", &params.max_keys.to_string());
    write_xml_element(&mut writer, "KeyCount", &params.key_count.to_string());
    write_xml_element(
        &mut writer,
        "IsTruncated",
        if params.is_truncated { "true" } else { "false" },
    );

    if let Some(delimiter) = params.delimiter {
        if !delimiter.is_empty() {
            write_xml_element(&mut writer, "Delimiter", delimiter);
        }
    }

    if let Some(token) = params.continuation_token {
        write_xml_element(&mut writer, "ContinuationToken", token);
    }

    if let Some(token) = params.next_continuation_token {
        write_xml_element(&mut writer, "NextContinuationToken", token);
    }

    if let Some(start_after) = params.start_after {
        write_xml_element(&mut writer, "StartAfter", start_after);
    }

    if let Some(encoding) = params.encoding_type {
        write_xml_element(&mut writer, "EncodingType", encoding);
    }

    // <Contents> entries
    for entry in params.contents {
        write_list_entry(&mut writer, entry);
    }

    // <CommonPrefixes>
    for prefix in params.common_prefixes {
        writer
            .write_event(Event::Start(BytesStart::new("CommonPrefixes")))
            .expect("write CommonPrefixes start");
        write_xml_element(&mut writer, "Prefix", prefix);
        writer
            .write_event(Event::End(BytesEnd::new("CommonPrefixes")))
            .expect("write CommonPrefixes end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("ListBucketResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `ListBucketResult` (ListObjects V1).
///
/// V1 uses `Marker`/`NextMarker` instead of `ContinuationToken`/`NextContinuationToken`,
/// and does not include `KeyCount`.
pub fn list_bucket_v1_result(params: &ListBucketV1ResultParams) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("ListBucketResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    write_xml_element(&mut writer, "Name", params.name);

    if let Some(prefix) = params.prefix {
        write_xml_element(&mut writer, "Prefix", prefix);
    } else {
        write_xml_element(&mut writer, "Prefix", "");
    }

    // V1: Marker (always present, empty string if not set)
    write_xml_element(&mut writer, "Marker", params.marker.unwrap_or(""));

    if let Some(next_marker) = params.next_marker {
        write_xml_element(&mut writer, "NextMarker", next_marker);
    }

    write_xml_element(&mut writer, "MaxKeys", &params.max_keys.to_string());
    write_xml_element(
        &mut writer,
        "IsTruncated",
        if params.is_truncated { "true" } else { "false" },
    );

    if let Some(delimiter) = params.delimiter {
        write_xml_element(&mut writer, "Delimiter", delimiter);
    }

    if let Some(encoding) = params.encoding_type {
        write_xml_element(&mut writer, "EncodingType", encoding);
    }

    // <Contents> entries
    for entry in params.contents {
        write_list_entry(&mut writer, entry);
    }

    // <CommonPrefixes>
    for prefix in params.common_prefixes {
        writer
            .write_event(Event::Start(BytesStart::new("CommonPrefixes")))
            .expect("write CommonPrefixes start");
        write_xml_element(&mut writer, "Prefix", prefix);
        writer
            .write_event(Event::End(BytesEnd::new("CommonPrefixes")))
            .expect("write CommonPrefixes end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("ListBucketResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `InitiateMultipartUploadResult`.
///
/// Produces XML like:
/// ```xml
/// <?xml version="1.0" encoding="UTF-8"?>
/// <InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
///   <Bucket>my-bucket</Bucket>
///   <Key>my-key</Key>
///   <UploadId>upload-id</UploadId>
/// </InitiateMultipartUploadResult>
/// ```
pub fn initiate_multipart_upload_result(bucket: &str, key: &str, upload_id: &str) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("InitiateMultipartUploadResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    write_xml_element(&mut writer, "Bucket", bucket);
    write_xml_element(&mut writer, "Key", key);
    write_xml_element(&mut writer, "UploadId", upload_id);

    writer
        .write_event(Event::End(BytesEnd::new("InitiateMultipartUploadResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `CompleteMultipartUploadResult`.
///
/// Produces XML like:
/// ```xml
/// <?xml version="1.0" encoding="UTF-8"?>
/// <CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
///   <Location>/bucket/key</Location>
///   <Bucket>bucket</Bucket>
///   <Key>key</Key>
///   <ETag>"etag"</ETag>
/// </CompleteMultipartUploadResult>
/// ```
pub fn complete_multipart_upload_result(bucket: &str, key: &str, etag: &str) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("CompleteMultipartUploadResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    let location = format!("/{bucket}/{key}");
    write_xml_element(&mut writer, "Location", &location);
    write_xml_element(&mut writer, "Bucket", bucket);
    write_xml_element(&mut writer, "Key", key);

    let quoted_etag = format!("\"{}\"", etag);
    write_xml_element(&mut writer, "ETag", &quoted_etag);

    writer
        .write_event(Event::End(BytesEnd::new("CompleteMultipartUploadResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `ListVersionsResult` (ListObjectVersions).
///
/// Accepts `ObjectRecord` entries with real version IDs, is_latest flags,
/// and delete markers. Entries are separated into `<Version>` and
/// `<DeleteMarker>` elements.
pub fn list_versions_result(
    name: &str,
    prefix: Option<&str>,
    key_marker: Option<&str>,
    version_id_marker: Option<&str>,
    max_keys: u32,
    is_truncated: bool,
    entries: &[crate::types::ObjectRecord],
    next_key_marker: Option<&str>,
    next_version_id_marker: Option<&str>,
) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("ListVersionsResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    write_xml_element(&mut writer, "Name", name);
    write_xml_element(&mut writer, "Prefix", prefix.unwrap_or(""));
    write_xml_element(&mut writer, "KeyMarker", key_marker.unwrap_or(""));
    write_xml_element(&mut writer, "VersionIdMarker", version_id_marker.unwrap_or(""));
    write_xml_element(&mut writer, "MaxKeys", &max_keys.to_string());
    write_xml_element(
        &mut writer,
        "IsTruncated",
        if is_truncated { "true" } else { "false" },
    );

    if let Some(next) = next_key_marker {
        write_xml_element(&mut writer, "NextKeyMarker", next);
        write_xml_element(
            &mut writer,
            "NextVersionIdMarker",
            next_version_id_marker.unwrap_or("null"),
        );
    }

    for entry in entries {
        let version_id = entry.version_id.as_deref().unwrap_or("null");
        let is_latest = if entry.is_latest { "true" } else { "false" };
        let date = entry
            .last_modified
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();

        if entry.is_delete_marker {
            writer
                .write_event(Event::Start(BytesStart::new("DeleteMarker")))
                .expect("write DeleteMarker start");

            write_xml_element(&mut writer, "Key", &entry.key);
            write_xml_element(&mut writer, "VersionId", version_id);
            write_xml_element(&mut writer, "IsLatest", is_latest);
            write_xml_element(&mut writer, "LastModified", &date);

            // Owner
            writer
                .write_event(Event::Start(BytesStart::new("Owner")))
                .expect("write Owner start");
            write_xml_element(&mut writer, "ID", &entry.owner);
            write_xml_element(&mut writer, "DisplayName", &entry.owner);
            writer
                .write_event(Event::End(BytesEnd::new("Owner")))
                .expect("write Owner end");

            writer
                .write_event(Event::End(BytesEnd::new("DeleteMarker")))
                .expect("write DeleteMarker end");
        } else {
            writer
                .write_event(Event::Start(BytesStart::new("Version")))
                .expect("write Version start");

            write_xml_element(&mut writer, "Key", &entry.key);
            write_xml_element(&mut writer, "VersionId", version_id);
            write_xml_element(&mut writer, "IsLatest", is_latest);

            write_xml_element(&mut writer, "LastModified", &date);

            let quoted_etag = format!("\"{}\"", entry.etag);
            write_xml_element(&mut writer, "ETag", &quoted_etag);

            write_xml_element(&mut writer, "Size", &entry.size.to_string());
            write_xml_element(&mut writer, "StorageClass", &entry.storage_class);

            // Owner
            writer
                .write_event(Event::Start(BytesStart::new("Owner")))
                .expect("write Owner start");
            write_xml_element(&mut writer, "ID", &entry.owner);
            write_xml_element(&mut writer, "DisplayName", &entry.owner);
            writer
                .write_event(Event::End(BytesEnd::new("Owner")))
                .expect("write Owner end");

            writer
                .write_event(Event::End(BytesEnd::new("Version")))
                .expect("write Version end");
        }
    }

    writer
        .write_event(Event::End(BytesEnd::new("ListVersionsResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `GetBucketVersioning`.
///
/// Returns `<VersioningConfiguration/>` when unversioned, or
/// `<VersioningConfiguration><Status>Enabled|Suspended</Status></VersioningConfiguration>`.
pub fn versioning_configuration_result(status: Option<&str>) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    match status {
        Some(s) => {
            let mut root = BytesStart::new("VersioningConfiguration");
            root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
            writer
                .write_event(Event::Start(root))
                .expect("write root start");
            write_xml_element(&mut writer, "Status", s);
            writer
                .write_event(Event::End(BytesEnd::new("VersioningConfiguration")))
                .expect("write root end");
        }
        None => {
            let mut root = BytesStart::new("VersioningConfiguration");
            root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
            writer
                .write_event(Event::Empty(root))
                .expect("write empty root");
        }
    }

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Builds the XML response for `GetBucketLocation`.
///
/// Per the S3 spec, an empty `LocationConstraint` element means `us-east-1`.
/// When `region` is `"us-east-1"`, we return an empty element for compatibility.
/// For any other region, we return the region as the element text.
pub fn location_constraint(region: &str) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut elem = BytesStart::new("LocationConstraint");
    elem.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));

    if region == "us-east-1" {
        // Empty element = us-east-1 (S3 convention)
        writer
            .write_event(Event::Empty(elem))
            .expect("write LocationConstraint");
    } else {
        writer
            .write_event(Event::Start(elem))
            .expect("write LocationConstraint start");
        writer
            .write_event(Event::Text(BytesText::new(region)))
            .expect("write region text");
        writer
            .write_event(Event::End(BytesEnd::new("LocationConstraint")))
            .expect("write LocationConstraint end");
    }

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// A successfully deleted key in a `DeleteObjects` response.
pub struct DeletedEntry {
    pub key: String,
    /// Version ID of the deleted version (or the new delete marker).
    pub version_id: Option<String>,
    /// True when a delete marker was created (non-versioned delete in versioned bucket).
    pub delete_marker: bool,
    /// Version ID of the delete marker (when delete_marker is true).
    pub delete_marker_version_id: Option<String>,
}

/// A failed key in a `DeleteObjects` response.
pub struct DeleteErrorEntry {
    pub key: String,
    pub version_id: Option<String>,
    pub code: String,
    pub message: String,
}

/// Builds the XML response for `DeleteResult` (DeleteObjects).
///
/// When `quiet` is true, only `<Error>` elements are included.
pub fn delete_objects_result(
    deleted: &[DeletedEntry],
    errors: &[DeleteErrorEntry],
    quiet: bool,
) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("DeleteResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    if !quiet {
        for entry in deleted {
            writer
                .write_event(Event::Start(BytesStart::new("Deleted")))
                .expect("write Deleted start");
            write_xml_element(&mut writer, "Key", &entry.key);
            if let Some(ref vid) = entry.version_id {
                write_xml_element(&mut writer, "VersionId", vid);
            }
            if entry.delete_marker {
                write_xml_element(&mut writer, "DeleteMarker", "true");
                if let Some(ref dm_vid) = entry.delete_marker_version_id {
                    write_xml_element(&mut writer, "DeleteMarkerVersionId", dm_vid);
                }
            }
            writer
                .write_event(Event::End(BytesEnd::new("Deleted")))
                .expect("write Deleted end");
        }
    }

    for entry in errors {
        writer
            .write_event(Event::Start(BytesStart::new("Error")))
            .expect("write Error start");
        write_xml_element(&mut writer, "Key", &entry.key);
        if let Some(ref vid) = entry.version_id {
            write_xml_element(&mut writer, "VersionId", vid);
        }
        write_xml_element(&mut writer, "Code", &entry.code);
        write_xml_element(&mut writer, "Message", &entry.message);
        writer
            .write_event(Event::End(BytesEnd::new("Error")))
            .expect("write Error end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("DeleteResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Writes a single `<Contents>` element for a list entry.
fn write_list_entry(writer: &mut Writer<Vec<u8>>, entry: &ListEntry) {
    writer
        .write_event(Event::Start(BytesStart::new("Contents")))
        .expect("write Contents start");

    write_xml_element(writer, "Key", &entry.key);

    let date = entry.last_modified.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    write_xml_element(writer, "LastModified", &date);

    let quoted_etag = format!("\"{}\"", entry.etag);
    write_xml_element(writer, "ETag", &quoted_etag);

    write_xml_element(writer, "Size", &entry.size.to_string());

    if let (Some(ref id), Some(ref name)) = (&entry.owner_id, &entry.owner_display_name) {
        writer
            .write_event(Event::Start(BytesStart::new("Owner")))
            .expect("write Owner start");
        write_xml_element(writer, "ID", id);
        write_xml_element(writer, "DisplayName", name);
        writer
            .write_event(Event::End(BytesEnd::new("Owner")))
            .expect("write Owner end");
    }

    write_xml_element(writer, "StorageClass", &entry.storage_class);

    writer
        .write_event(Event::End(BytesEnd::new("Contents")))
        .expect("write Contents end");
}

/// Builds the XML response for `ListMultipartUploadsResult`.
pub fn list_multipart_uploads_result(
    bucket: &str,
    prefix: Option<&str>,
    key_marker: Option<&str>,
    upload_id_marker: Option<&str>,
    max_uploads: u32,
    is_truncated: bool,
    uploads: &[MultipartUploadRecord],
    next_key_marker: Option<&str>,
    next_upload_id_marker: Option<&str>,
    owner: &str,
) -> String {
    let mut writer = Writer::new(Vec::new());

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("ListMultipartUploadsResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write root start");

    write_xml_element(&mut writer, "Bucket", bucket);
    write_xml_element(&mut writer, "KeyMarker", key_marker.unwrap_or(""));
    write_xml_element(
        &mut writer,
        "UploadIdMarker",
        upload_id_marker.unwrap_or(""),
    );

    if let Some(next) = next_key_marker {
        write_xml_element(&mut writer, "NextKeyMarker", next);
    }
    if let Some(next) = next_upload_id_marker {
        write_xml_element(&mut writer, "NextUploadIdMarker", next);
    }

    write_xml_element(&mut writer, "MaxUploads", &max_uploads.to_string());
    write_xml_element(
        &mut writer,
        "IsTruncated",
        if is_truncated { "true" } else { "false" },
    );

    if let Some(pfx) = prefix {
        write_xml_element(&mut writer, "Prefix", pfx);
    } else {
        write_xml_element(&mut writer, "Prefix", "");
    }

    for upload in uploads {
        writer
            .write_event(Event::Start(BytesStart::new("Upload")))
            .expect("write Upload start");

        write_xml_element(&mut writer, "Key", &upload.key);
        write_xml_element(&mut writer, "UploadId", &upload.upload_id);

        writer
            .write_event(Event::Start(BytesStart::new("Initiator")))
            .expect("write Initiator start");
        write_xml_element(&mut writer, "ID", owner);
        write_xml_element(&mut writer, "DisplayName", owner);
        writer
            .write_event(Event::End(BytesEnd::new("Initiator")))
            .expect("write Initiator end");

        writer
            .write_event(Event::Start(BytesStart::new("Owner")))
            .expect("write Owner start");
        write_xml_element(&mut writer, "ID", owner);
        write_xml_element(&mut writer, "DisplayName", owner);
        writer
            .write_event(Event::End(BytesEnd::new("Owner")))
            .expect("write Owner end");

        write_xml_element(&mut writer, "StorageClass", "STANDARD");

        let date = upload
            .initiated_at
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        write_xml_element(&mut writer, "Initiated", &date);

        writer
            .write_event(Event::End(BytesEnd::new("Upload")))
            .expect("write Upload end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("ListMultipartUploadsResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

// -- Tagging XML types --

/// Serialize a tag set into S3 Tagging XML response.
pub fn tagging_result(tags: &[(String, String)]) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("Tagging");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer
        .write_event(Event::Start(root))
        .expect("write Tagging start");

    writer
        .write_event(Event::Start(BytesStart::new("TagSet")))
        .expect("write TagSet start");

    for (key, value) in tags {
        writer
            .write_event(Event::Start(BytesStart::new("Tag")))
            .expect("write Tag start");
        write_xml_element(&mut writer, "Key", key);
        write_xml_element(&mut writer, "Value", value);
        writer
            .write_event(Event::End(BytesEnd::new("Tag")))
            .expect("write Tag end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("TagSet")))
        .expect("write TagSet end");
    writer
        .write_event(Event::End(BytesEnd::new("Tagging")))
        .expect("write Tagging end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Parse an S3 Tagging XML request body into a list of (key, value) pairs.
pub fn parse_tagging_xml(xml: &str) -> Result<Vec<(String, String)>, crate::error::S3Error> {
    use crate::error::{S3Error, S3ErrorCode};

    // Deserialize with quick_xml + serde
    #[derive(serde::Deserialize)]
    #[serde(rename = "Tagging")]
    struct TaggingBody {
        #[serde(rename = "TagSet")]
        tag_set: TagSetBody,
    }
    #[derive(serde::Deserialize)]
    struct TagSetBody {
        #[serde(rename = "Tag", default)]
        tags: Vec<TagBody>,
    }
    #[derive(serde::Deserialize)]
    struct TagBody {
        #[serde(rename = "Key")]
        key: String,
        #[serde(rename = "Value")]
        value: String,
    }

    let body: TaggingBody = quick_xml::de::from_str(xml).map_err(|e| {
        S3Error::with_message(S3ErrorCode::MalformedXML, format!("Invalid tagging XML: {e}"), "")
    })?;

    let tags: Vec<(String, String)> = body
        .tag_set
        .tags
        .into_iter()
        .map(|t| (t.key, t.value))
        .collect();

    validate_tags(&tags)?;
    Ok(tags)
}

/// Parse the `x-amz-tagging` header value (URL-encoded key=value pairs).
pub fn parse_tagging_header(header: &str) -> Result<Vec<(String, String)>, crate::error::S3Error> {
    let tags: Vec<(String, String)> = form_urlencoded::parse(header.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    validate_tags(&tags)?;
    Ok(tags)
}

/// Validate tag constraints: max 10 tags, key max 128 chars, value max 256 chars, no duplicates.
fn validate_tags(tags: &[(String, String)]) -> Result<(), crate::error::S3Error> {
    use crate::error::{S3Error, S3ErrorCode};

    if tags.len() > 10 {
        return Err(S3Error::with_message(
            S3ErrorCode::InvalidTag,
            "Object tags cannot be greater than 10",
            "",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (key, value) in tags {
        if key.len() > 128 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidTag,
                "The TagKey you have provided is too long, max 128 chars",
                "",
            ));
        }
        if value.len() > 256 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidTag,
                "The TagValue you have provided is too long, max 256 chars",
                "",
            ));
        }
        if key.is_empty() {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidTag,
                "The TagKey cannot be empty",
                "",
            ));
        }
        if !seen.insert(key.as_str()) {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidTag,
                format!("Cannot provide multiple Tags with the same key: {key}"),
                "",
            ));
        }
    }
    Ok(())
}

/// Build a ListPartsResult XML response.
pub fn list_parts_result(
    bucket: &str,
    key: &str,
    upload_id: &str,
    owner: &str,
    storage_class: &str,
    part_number_marker: u32,
    next_part_number_marker: Option<u32>,
    max_parts: u32,
    is_truncated: bool,
    parts: &[PartRecord],
    checksum_algorithm: Option<&str>,
) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("ListPartsResult");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer.write_event(Event::Start(root)).expect("write root");

    write_xml_element(&mut writer, "Bucket", bucket);
    write_xml_element(&mut writer, "Key", key);
    write_xml_element(&mut writer, "UploadId", upload_id);

    // Initiator
    writer
        .write_event(Event::Start(BytesStart::new("Initiator")))
        .expect("write Initiator");
    write_xml_element(&mut writer, "ID", owner);
    write_xml_element(&mut writer, "DisplayName", owner);
    writer
        .write_event(Event::End(BytesEnd::new("Initiator")))
        .expect("write Initiator end");

    // Owner
    writer
        .write_event(Event::Start(BytesStart::new("Owner")))
        .expect("write Owner");
    write_xml_element(&mut writer, "ID", owner);
    write_xml_element(&mut writer, "DisplayName", owner);
    writer
        .write_event(Event::End(BytesEnd::new("Owner")))
        .expect("write Owner end");

    write_xml_element(&mut writer, "StorageClass", storage_class);
    write_xml_element(
        &mut writer,
        "PartNumberMarker",
        &part_number_marker.to_string(),
    );
    if let Some(next) = next_part_number_marker {
        write_xml_element(&mut writer, "NextPartNumberMarker", &next.to_string());
    }
    write_xml_element(&mut writer, "MaxParts", &max_parts.to_string());
    write_xml_element(
        &mut writer,
        "IsTruncated",
        if is_truncated { "true" } else { "false" },
    );

    if let Some(algo) = checksum_algorithm {
        write_xml_element(&mut writer, "ChecksumAlgorithm", algo);
    }

    for part in parts {
        writer
            .write_event(Event::Start(BytesStart::new("Part")))
            .expect("write Part");
        write_xml_element(&mut writer, "PartNumber", &part.part_number.to_string());

        // LastModified: use part's last_modified if available, otherwise empty
        if let Some(ref lm) = part.last_modified {
            write_xml_element(&mut writer, "LastModified", &lm.to_rfc3339());
        }

        let quoted_etag = if part.etag.starts_with('"') {
            part.etag.clone()
        } else {
            format!("\"{}\"", part.etag)
        };
        write_xml_element(&mut writer, "ETag", &quoted_etag);
        write_xml_element(&mut writer, "Size", &part.size.to_string());

        // Part checksum
        if let (Some(algo), Some(ref val)) = (checksum_algorithm, &part.checksum_value) {
            let tag = format!("Checksum{}", algo.to_uppercase());
            write_xml_element(&mut writer, &tag, val);
        }

        writer
            .write_event(Event::End(BytesEnd::new("Part")))
            .expect("write Part end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("ListPartsResult")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

/// Requested attributes for GetObjectAttributes.
pub struct ObjectAttributesRequest {
    pub etag: bool,
    pub checksum: bool,
    pub object_parts: bool,
    pub storage_class: bool,
    pub object_size: bool,
}

/// Build a GetObjectAttributesResponse XML.
pub fn get_object_attributes_result(
    etag: Option<&str>,
    checksum_algorithm: Option<&str>,
    checksum_value: Option<&str>,
    parts_count: Option<u32>,
    storage_class: Option<&str>,
    object_size: Option<u64>,
) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("GetObjectAttributesResponse");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer.write_event(Event::Start(root)).expect("write root");

    // ETag (without quotes, per S3 spec for this API)
    if let Some(etag) = etag {
        let unquoted = etag.trim_matches('"');
        write_xml_element(&mut writer, "ETag", unquoted);
    }

    // Checksum
    if let (Some(algo), Some(val)) = (checksum_algorithm, checksum_value) {
        writer
            .write_event(Event::Start(BytesStart::new("Checksum")))
            .expect("write Checksum");
        let tag = format!("Checksum{}", algo.to_uppercase());
        write_xml_element(&mut writer, &tag, val);
        writer
            .write_event(Event::End(BytesEnd::new("Checksum")))
            .expect("write Checksum end");
    }

    // ObjectParts
    if let Some(count) = parts_count {
        writer
            .write_event(Event::Start(BytesStart::new("ObjectParts")))
            .expect("write ObjectParts");
        write_xml_element(&mut writer, "TotalPartsCount", &count.to_string());
        writer
            .write_event(Event::End(BytesEnd::new("ObjectParts")))
            .expect("write ObjectParts end");
    }

    // StorageClass
    if let Some(sc) = storage_class {
        write_xml_element(&mut writer, "StorageClass", sc);
    }

    // ObjectSize
    if let Some(size) = object_size {
        write_xml_element(&mut writer, "ObjectSize", &size.to_string());
    }

    writer
        .write_event(Event::End(BytesEnd::new("GetObjectAttributesResponse")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn empty_bucket_list() {
        let xml = list_all_my_buckets_result(&[], "root");

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<ListAllMyBucketsResult"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        assert!(xml.contains("<Owner>"));
        assert!(xml.contains("<ID>root</ID>"));
        assert!(xml.contains("<DisplayName>root</DisplayName>"));
        assert!(xml.contains("<Buckets/>") || xml.contains("<Buckets></Buckets>"));
    }

    #[test]
    fn single_bucket() {
        let buckets = vec![BucketInfo {
            name: "my-bucket".to_string(),
            created_at: chrono::Utc.with_ymd_and_hms(2024, 1, 15, 10, 30, 0).unwrap(),
            owner: "root".to_string(),
        }];
        let xml = list_all_my_buckets_result(&buckets, "root");

        assert!(xml.contains("<Name>my-bucket</Name>"));
        assert!(xml.contains("<CreationDate>2024-01-15T10:30:00.000Z</CreationDate>"));
    }

    #[test]
    fn multiple_buckets() {
        let buckets = vec![
            BucketInfo {
                name: "alpha".to_string(),
                created_at: chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
                owner: "root".to_string(),
            },
            BucketInfo {
                name: "beta".to_string(),
                created_at: chrono::Utc.with_ymd_and_hms(2024, 6, 15, 12, 0, 0).unwrap(),
                owner: "root".to_string(),
            },
        ];
        let xml = list_all_my_buckets_result(&buckets, "root");

        assert!(xml.contains("<Name>alpha</Name>"));
        assert!(xml.contains("<Name>beta</Name>"));
        // Both should have Bucket elements
        assert_eq!(xml.matches("<Bucket>").count(), 2);
    }

    // -- CopyObjectResult tests --

    #[test]
    fn copy_object_result_basic() {
        let dt = chrono::Utc.with_ymd_and_hms(2024, 3, 15, 10, 30, 0).unwrap();
        let xml = copy_object_result("abc123", &dt);

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<CopyObjectResult"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        assert!(xml.contains("<ETag>&quot;abc123&quot;</ETag>"));
        assert!(xml.contains("<LastModified>2024-03-15T10:30:00.000Z</LastModified>"));
    }

    // -- ListBucketResult tests --

    #[test]
    fn list_bucket_result_empty() {
        let params = ListBucketResultParams {
            name: "my-bucket",
            prefix: None,
            delimiter: None,
            max_keys: 1000,
            is_truncated: false,
            key_count: 0,
            contents: &[],
            common_prefixes: &[],
            continuation_token: None,
            next_continuation_token: None,
            start_after: None,
            encoding_type: None,
            fetch_owner: false,
        };
        let xml = list_bucket_result(&params);

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<ListBucketResult"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        assert!(xml.contains("<Name>my-bucket</Name>"));
        assert!(xml.contains("<MaxKeys>1000</MaxKeys>"));
        assert!(xml.contains("<KeyCount>0</KeyCount>"));
        assert!(xml.contains("<IsTruncated>false</IsTruncated>"));
        assert!(xml.contains("<Prefix></Prefix>") || xml.contains("<Prefix/>"));
        assert!(!xml.contains("<Contents>"));
    }

    #[test]
    fn list_bucket_result_with_contents() {
        let dt = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let entries = vec![ListEntry {
            key: "file.txt".to_string(),
            last_modified: dt,
            etag: "abc123".to_string(),
            size: 42,
            storage_class: "STANDARD".to_string(),
            owner_id: None,
            owner_display_name: None,
        }];
        let params = ListBucketResultParams {
            name: "my-bucket",
            prefix: Some("f"),
            delimiter: None,
            max_keys: 1000,
            is_truncated: false,
            key_count: 1,
            contents: &entries,
            common_prefixes: &[],
            continuation_token: None,
            next_continuation_token: None,
            start_after: None,
            encoding_type: None,
            fetch_owner: false,
        };
        let xml = list_bucket_result(&params);

        assert!(xml.contains("<Contents>"));
        assert!(xml.contains("<Key>file.txt</Key>"));
        assert!(xml.contains("<ETag>&quot;abc123&quot;</ETag>"));
        assert!(xml.contains("<Size>42</Size>"));
        assert!(xml.contains("<StorageClass>STANDARD</StorageClass>"));
        assert!(xml.contains("<Prefix>f</Prefix>"));
        assert!(xml.contains("<KeyCount>1</KeyCount>"));
    }

    #[test]
    fn list_bucket_result_with_common_prefixes() {
        let params = ListBucketResultParams {
            name: "my-bucket",
            prefix: None,
            delimiter: Some("/"),
            max_keys: 1000,
            is_truncated: false,
            key_count: 2,
            contents: &[],
            common_prefixes: &["photos/".to_string(), "videos/".to_string()],
            continuation_token: None,
            next_continuation_token: None,
            start_after: None,
            encoding_type: None,
            fetch_owner: false,
        };
        let xml = list_bucket_result(&params);

        assert!(xml.contains("<Delimiter>/</Delimiter>"));
        assert_eq!(xml.matches("<CommonPrefixes>").count(), 2);
        assert!(xml.contains("<Prefix>photos/</Prefix>"));
        assert!(xml.contains("<Prefix>videos/</Prefix>"));
    }

    #[test]
    fn list_bucket_result_truncated_with_token() {
        let params = ListBucketResultParams {
            name: "my-bucket",
            prefix: None,
            delimiter: None,
            max_keys: 2,
            is_truncated: true,
            key_count: 2,
            contents: &[],
            common_prefixes: &[],
            continuation_token: Some("prev-token"),
            next_continuation_token: Some("next-token"),
            start_after: None,
            encoding_type: None,
            fetch_owner: false,
        };
        let xml = list_bucket_result(&params);

        assert!(xml.contains("<IsTruncated>true</IsTruncated>"));
        assert!(xml.contains("<ContinuationToken>prev-token</ContinuationToken>"));
        assert!(xml.contains("<NextContinuationToken>next-token</NextContinuationToken>"));
    }

    #[test]
    fn list_bucket_result_with_start_after() {
        let params = ListBucketResultParams {
            name: "my-bucket",
            prefix: None,
            delimiter: None,
            max_keys: 1000,
            is_truncated: false,
            key_count: 0,
            contents: &[],
            common_prefixes: &[],
            continuation_token: None,
            next_continuation_token: None,
            start_after: Some("key-abc"),
            encoding_type: None,
            fetch_owner: false,
        };
        let xml = list_bucket_result(&params);

        assert!(xml.contains("<StartAfter>key-abc</StartAfter>"));
    }

    // -- InitiateMultipartUploadResult tests --

    #[test]
    fn initiate_multipart_upload_result_basic() {
        let xml = initiate_multipart_upload_result("my-bucket", "my-key", "upload-123");

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<InitiateMultipartUploadResult"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        assert!(xml.contains("<Bucket>my-bucket</Bucket>"));
        assert!(xml.contains("<Key>my-key</Key>"));
        assert!(xml.contains("<UploadId>upload-123</UploadId>"));
    }

    // -- CompleteMultipartUploadResult tests --

    #[test]
    fn complete_multipart_upload_result_basic() {
        let xml = complete_multipart_upload_result("my-bucket", "my-key", "abc123-2");

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<CompleteMultipartUploadResult"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        assert!(xml.contains("<Location>/my-bucket/my-key</Location>"));
        assert!(xml.contains("<Bucket>my-bucket</Bucket>"));
        assert!(xml.contains("<Key>my-key</Key>"));
        assert!(xml.contains("<ETag>&quot;abc123-2&quot;</ETag>"));
    }

    // -- CompleteMultipartUpload XML parser tests --

    #[test]
    fn parse_complete_multipart_upload_basic() {
        let xml = r#"<CompleteMultipartUpload>
            <Part><PartNumber>1</PartNumber><ETag>"aaa"</ETag></Part>
            <Part><PartNumber>2</PartNumber><ETag>"bbb"</ETag></Part>
        </CompleteMultipartUpload>"#;

        let body = parse_complete_multipart_upload(xml).unwrap();
        assert_eq!(body.parts.len(), 2);
        assert_eq!(body.parts[0].part_number, 1);
        assert_eq!(body.parts[0].etag, "\"aaa\"");
        assert_eq!(body.parts[1].part_number, 2);
        assert_eq!(body.parts[1].etag, "\"bbb\"");
    }

    #[test]
    fn parse_complete_multipart_upload_single_part() {
        let xml = r#"<CompleteMultipartUpload>
            <Part><PartNumber>1</PartNumber><ETag>"abc"</ETag></Part>
        </CompleteMultipartUpload>"#;

        let body = parse_complete_multipart_upload(xml).unwrap();
        assert_eq!(body.parts.len(), 1);
        assert_eq!(body.parts[0].part_number, 1);
    }

    // -- DeleteObjects XML parser tests --

    #[test]
    fn parse_delete_objects_basic() {
        let xml = r#"<Delete>
            <Quiet>false</Quiet>
            <Object><Key>key1</Key></Object>
            <Object><Key>key2</Key></Object>
        </Delete>"#;

        let body = parse_delete_objects(xml).unwrap();
        assert!(!body.quiet);
        assert_eq!(body.objects.len(), 2);
        assert_eq!(body.objects[0].key, "key1");
        assert_eq!(body.objects[1].key, "key2");
    }

    #[test]
    fn parse_delete_objects_quiet() {
        let xml = r#"<Delete>
            <Quiet>true</Quiet>
            <Object><Key>only-key</Key></Object>
        </Delete>"#;

        let body = parse_delete_objects(xml).unwrap();
        assert!(body.quiet);
        assert_eq!(body.objects.len(), 1);
    }

    #[test]
    fn parse_delete_objects_preserves_whitespace_key() {
        let xml = r#"<Delete>
            <Object><Key> </Key></Object>
        </Delete>"#;

        let body = parse_delete_objects(xml).unwrap();
        assert_eq!(body.objects.len(), 1);
        assert_eq!(body.objects[0].key, " ");
    }

    #[test]
    fn parse_delete_objects_no_quiet_defaults_false() {
        let xml = r#"<Delete>
            <Object><Key>k</Key></Object>
        </Delete>"#;

        let body = parse_delete_objects(xml).unwrap();
        assert!(!body.quiet);
    }

    #[test]
    fn parse_delete_objects_with_version_id() {
        let xml = r#"<Delete>
            <Quiet>true</Quiet>
            <Object><Key>key1</Key><VersionId>vid-1</VersionId></Object>
            <Object><Key>key2</Key></Object>
        </Delete>"#;

        let body = parse_delete_objects(xml).unwrap();
        assert!(body.quiet);
        assert_eq!(body.objects.len(), 2);
        assert_eq!(body.objects[0].key, "key1");
        assert_eq!(body.objects[0].version_id.as_deref(), Some("vid-1"));
        assert_eq!(body.objects[1].key, "key2");
        assert!(body.objects[1].version_id.is_none());
    }

    // -- DeleteResult XML builder tests --

    #[test]
    fn delete_objects_result_verbose() {
        let deleted = vec![
            DeletedEntry { key: "a.txt".to_string(), version_id: None, delete_marker: false, delete_marker_version_id: None },
            DeletedEntry { key: "b.txt".to_string(), version_id: None, delete_marker: false, delete_marker_version_id: None },
        ];
        let errors = vec![DeleteErrorEntry {
            key: "c.txt".to_string(),
            version_id: None,
            code: "InternalError".to_string(),
            message: "oops".to_string(),
        }];
        let xml = delete_objects_result(&deleted, &errors, false);

        assert!(xml.contains("<DeleteResult"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        assert_eq!(xml.matches("<Deleted>").count(), 2);
        assert!(xml.contains("<Key>a.txt</Key>"));
        assert!(xml.contains("<Key>b.txt</Key>"));
        assert_eq!(xml.matches("<Error>").count(), 1);
        assert!(xml.contains("<Key>c.txt</Key>"));
        assert!(xml.contains("<Code>InternalError</Code>"));
        assert!(xml.contains("<Message>oops</Message>"));
    }

    #[test]
    fn delete_objects_result_quiet_omits_deleted() {
        let deleted = vec![DeletedEntry { key: "a.txt".to_string(), version_id: None, delete_marker: false, delete_marker_version_id: None }];
        let errors: Vec<DeleteErrorEntry> = vec![];
        let xml = delete_objects_result(&deleted, &errors, true);

        assert!(!xml.contains("<Deleted>"));
        assert!(!xml.contains("<Key>a.txt</Key>"));
    }

    #[test]
    fn location_constraint_us_east_1() {
        let xml = location_constraint("us-east-1");
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<LocationConstraint"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        // us-east-1 returns empty element (S3 convention)
        assert!(xml.contains("/>"));
        assert!(!xml.contains("us-east-1"));
    }

    #[test]
    fn location_constraint_custom_region() {
        let xml = location_constraint("eu-west-1");
        assert!(xml.contains(">eu-west-1</LocationConstraint>"));
    }

    #[test]
    fn delete_objects_result_quiet_includes_errors() {
        let deleted = vec![DeletedEntry { key: "a.txt".to_string(), version_id: None, delete_marker: false, delete_marker_version_id: None }];
        let errors = vec![DeleteErrorEntry {
            key: "b.txt".to_string(),
            version_id: None,
            code: "AccessDenied".to_string(),
            message: "denied".to_string(),
        }];
        let xml = delete_objects_result(&deleted, &errors, true);

        assert!(!xml.contains("<Key>a.txt</Key>"));
        assert!(xml.contains("<Key>b.txt</Key>"));
        assert!(xml.contains("<Code>AccessDenied</Code>"));
    }

    #[test]
    fn delete_objects_result_with_version_info() {
        let deleted = vec![
            DeletedEntry {
                key: "a.txt".to_string(),
                version_id: Some("vid-123".to_string()),
                delete_marker: false,
                delete_marker_version_id: None,
            },
            DeletedEntry {
                key: "b.txt".to_string(),
                version_id: None,
                delete_marker: true,
                delete_marker_version_id: Some("dm-456".to_string()),
            },
        ];
        let xml = delete_objects_result(&deleted, &[], false);

        // First entry: version ID, no delete marker.
        assert!(xml.contains("<VersionId>vid-123</VersionId>"));
        // Second entry: delete marker with its version ID.
        assert!(xml.contains("<DeleteMarker>true</DeleteMarker>"));
        assert!(xml.contains("<DeleteMarkerVersionId>dm-456</DeleteMarkerVersionId>"));
    }
}
