//! S3 XML request/response types.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;

use crate::error::write_xml_element;
use crate::types::{BucketInfo, ListBucketResultParams, ListEntry};

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
}
