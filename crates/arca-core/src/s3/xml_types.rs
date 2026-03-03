//! S3 XML request/response types.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;

use crate::error::write_xml_element;
use crate::types::{BucketInfo, ListBucketResultParams, ListEntry};

// -- Multipart upload XML types --

/// Parsed body of a `CompleteMultipartUpload` request.
#[derive(Debug, serde::Deserialize)]
#[serde(rename = "CompleteMultipartUpload")]
pub struct CompleteMultipartUploadBody {
    #[serde(rename = "Part")]
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
}

/// Parses a `DeleteObjects` XML request body.
pub fn parse_delete_objects(xml: &str) -> Result<DeleteObjectsBody, quick_xml::DeError> {
    quick_xml::de::from_str(xml)
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
pub fn list_all_my_buckets_result(buckets: &[BucketInfo]) -> String {
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
    write_xml_element(&mut writer, "ID", "arca");
    write_xml_element(&mut writer, "DisplayName", "arca");
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
        write_xml_element(&mut writer, "Delimiter", delimiter);
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

/// A successfully deleted key in a `DeleteObjects` response.
pub struct DeletedEntry {
    pub key: String,
}

/// A failed key in a `DeleteObjects` response.
pub struct DeleteErrorEntry {
    pub key: String,
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
    write_xml_element(writer, "StorageClass", &entry.storage_class);

    writer
        .write_event(Event::End(BytesEnd::new("Contents")))
        .expect("write Contents end");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn empty_bucket_list() {
        let xml = list_all_my_buckets_result(&[]);

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<ListAllMyBucketsResult"));
        assert!(xml.contains("xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\""));
        assert!(xml.contains("<Owner>"));
        assert!(xml.contains("<ID>arca</ID>"));
        assert!(xml.contains("<DisplayName>arca</DisplayName>"));
        assert!(xml.contains("<Buckets/>") || xml.contains("<Buckets></Buckets>"));
    }

    #[test]
    fn single_bucket() {
        let buckets = vec![BucketInfo {
            name: "my-bucket".to_string(),
            created_at: chrono::Utc.with_ymd_and_hms(2024, 1, 15, 10, 30, 0).unwrap(),
        }];
        let xml = list_all_my_buckets_result(&buckets);

        assert!(xml.contains("<Name>my-bucket</Name>"));
        assert!(xml.contains("<CreationDate>2024-01-15T10:30:00.000Z</CreationDate>"));
    }

    #[test]
    fn multiple_buckets() {
        let buckets = vec![
            BucketInfo {
                name: "alpha".to_string(),
                created_at: chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            },
            BucketInfo {
                name: "beta".to_string(),
                created_at: chrono::Utc.with_ymd_and_hms(2024, 6, 15, 12, 0, 0).unwrap(),
            },
        ];
        let xml = list_all_my_buckets_result(&buckets);

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
    fn parse_delete_objects_no_quiet_defaults_false() {
        let xml = r#"<Delete>
            <Object><Key>k</Key></Object>
        </Delete>"#;

        let body = parse_delete_objects(xml).unwrap();
        assert!(!body.quiet);
    }

    // -- DeleteResult XML builder tests --

    #[test]
    fn delete_objects_result_verbose() {
        let deleted = vec![
            DeletedEntry { key: "a.txt".to_string() },
            DeletedEntry { key: "b.txt".to_string() },
        ];
        let errors = vec![DeleteErrorEntry {
            key: "c.txt".to_string(),
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
        let deleted = vec![DeletedEntry { key: "a.txt".to_string() }];
        let errors: Vec<DeleteErrorEntry> = vec![];
        let xml = delete_objects_result(&deleted, &errors, true);

        assert!(!xml.contains("<Deleted>"));
        assert!(!xml.contains("<Key>a.txt</Key>"));
    }

    #[test]
    fn delete_objects_result_quiet_includes_errors() {
        let deleted = vec![DeletedEntry { key: "a.txt".to_string() }];
        let errors = vec![DeleteErrorEntry {
            key: "b.txt".to_string(),
            code: "AccessDenied".to_string(),
            message: "denied".to_string(),
        }];
        let xml = delete_objects_result(&deleted, &errors, true);

        assert!(!xml.contains("<Key>a.txt</Key>"));
        assert!(xml.contains("<Key>b.txt</Key>"));
        assert!(xml.contains("<Code>AccessDenied</Code>"));
    }
}
