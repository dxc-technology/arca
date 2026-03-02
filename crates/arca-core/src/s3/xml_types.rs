//! S3 XML request/response types.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;

use crate::error::write_xml_element;
use crate::types::BucketInfo;

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
}
