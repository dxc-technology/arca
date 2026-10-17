//! The part layout of a multipart object (TD-046): what `?partNumber=N`
//! selects, and the pages of `ObjectParts` in GetObjectAttributes.

use arca_core::store::ByteRange;
use arca_core::types::{ObjectPart, ObjectRecord};

/// `partNumber` bounds, as for UploadPart.
pub const MAX_PART_NUMBER: u32 = 10_000;

/// The default and the maximum `MaxParts` of GetObjectAttributes.
pub const DEFAULT_MAX_PARTS: u32 = 1000;

/// What `GET` / `HEAD` with `?partNumber=N` returns.
#[derive(Debug, PartialEq, Eq)]
pub struct PartSelection {
    /// The bytes of the part; `None` is the whole object (part 1 of an object
    /// that was not assembled from parts).
    pub range: Option<ByteRange>,
    /// `x-amz-mp-parts-count`, for an object assembled from parts.
    pub parts_count: Option<u32>,
    /// The part's own checksum (in the object's algorithm), when it has one.
    pub checksum: Option<String>,
}

/// Resolves `?partNumber=N` against `record`. The error is the message of the
/// `InvalidPart` answer: N is out of range, or the layout of the object was
/// never recorded (a multipart object completed by an older version).
pub fn select_part(record: &ObjectRecord, part_number: u32) -> Result<PartSelection, String> {
    let Some(parts) = &record.parts else {
        // Not assembled from parts (or from an older version that did not
        // keep the layout): an object with an ETag of a multipart upload
        // (`<md5>-<count>`) has parts we cannot tell apart.
        if record.etag.contains('-') {
            return Err(
                "The part layout of this object was not recorded (it was completed by an older version)"
                    .to_string(),
            );
        }
        return if part_number == 1 {
            Ok(PartSelection { range: None, parts_count: None, checksum: None })
        } else {
            Err("The requested partnumber is not satisfiable".to_string())
        };
    };
    let count = u32::try_from(parts.len()).unwrap_or(u32::MAX);
    let index = part_number
        .checked_sub(1)
        .map(|i| i as usize)
        .filter(|i| *i < parts.len())
        .ok_or_else(|| "The requested partnumber is not satisfiable".to_string())?;
    let start: u64 = parts[..index].iter().map(|p| p.size).sum();
    let part = &parts[index];
    Ok(PartSelection {
        // An empty part has no bytes to range over.
        range: (part.size > 0).then(|| ByteRange {
            start,
            end: Some(start + part.size - 1),
        }),
        parts_count: Some(count),
        checksum: part.checksum.clone(),
    })
}

/// One page of the parts of an object, `max_parts` parts after the part number
/// `marker` (the way ListParts pages).
#[derive(Debug, PartialEq, Eq)]
pub struct PartsPage<'a> {
    pub total: u32,
    pub parts: Vec<&'a ObjectPart>,
    /// The marker to continue from, when the page is truncated.
    pub next_marker: Option<u32>,
}

pub fn parts_page(parts: &[ObjectPart], marker: u32, max_parts: u32) -> PartsPage<'_> {
    let remaining: Vec<&ObjectPart> = parts.iter().filter(|p| p.part_number > marker).collect();
    let take = max_parts as usize;
    let truncated = remaining.len() > take;
    let page: Vec<&ObjectPart> = remaining.into_iter().take(take).collect();
    let next_marker = truncated.then(|| page.last().map_or(marker, |p| p.part_number));
    PartsPage {
        total: u32::try_from(parts.len()).unwrap_or(u32::MAX),
        parts: page,
        next_marker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::types::BlobId;
    use chrono::Utc;
    use std::collections::HashMap;

    fn record(etag: &str, parts: Option<Vec<ObjectPart>>) -> ObjectRecord {
        ObjectRecord {
            bucket: "b".into(),
            key: "k".into(),
            blob_id: BlobId::new(),
            size: parts.as_ref().map_or(4, |p| p.iter().map(|x| x.size).sum()),
            etag: etag.into(),
            content_type: None,
            last_modified: Utc::now(),
            metadata: HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".into(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".into(),
            checksum_algorithm: None,
            checksum_value: None,
            parts,
            replication_status: None,
            lock_updated_at: None,
            content_updated_at: None,
        }
    }

    fn layout(sizes: &[u64]) -> Vec<ObjectPart> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, size)| ObjectPart {
                part_number: i as u32 + 1,
                size: *size,
                checksum: Some(format!("c{}", i + 1)),
            })
            .collect()
    }

    #[test]
    fn each_part_is_the_range_after_the_ones_before_it() {
        let r = record("e-3", Some(layout(&[10, 20, 5])));
        let one = select_part(&r, 1).unwrap();
        assert_eq!(one.range, Some(ByteRange { start: 0, end: Some(9) }));
        assert_eq!(one.parts_count, Some(3));
        assert_eq!(one.checksum.as_deref(), Some("c1"));
        let two = select_part(&r, 2).unwrap();
        assert_eq!(two.range, Some(ByteRange { start: 10, end: Some(29) }));
        let three = select_part(&r, 3).unwrap();
        assert_eq!(three.range, Some(ByteRange { start: 30, end: Some(34) }));
        assert_eq!(three.checksum.as_deref(), Some("c3"));
    }

    #[test]
    fn a_part_beyond_the_last_or_zero_is_not_satisfiable() {
        let r = record("e-2", Some(layout(&[10, 20])));
        assert!(select_part(&r, 3).is_err());
        assert!(select_part(&r, 0).is_err());
        assert!(select_part(&r, u32::MAX).is_err());
    }

    #[test]
    fn a_part_without_a_checksum_selects_none() {
        let mut parts = layout(&[10, 20]);
        parts[1].checksum = None;
        let r = record("e-2", Some(parts));
        assert_eq!(select_part(&r, 2).unwrap().checksum, None);
    }

    #[test]
    fn an_empty_part_has_no_range() {
        let r = record("e-2", Some(layout(&[10, 0])));
        let two = select_part(&r, 2).unwrap();
        assert_eq!(two.range, None);
        assert_eq!(two.parts_count, Some(2));
    }

    #[test]
    fn a_plain_object_is_its_own_part_one() {
        let r = record("abc123", None);
        assert_eq!(
            select_part(&r, 1).unwrap(),
            PartSelection { range: None, parts_count: None, checksum: None }
        );
        assert!(select_part(&r, 2).is_err());
    }

    #[test]
    fn a_multipart_object_without_a_layout_cannot_be_split() {
        let r = record("abc123-4", None);
        let err = select_part(&r, 1).unwrap_err();
        assert!(err.contains("not recorded"), "{err}");
        assert!(select_part(&r, 3).is_err());
    }

    #[test]
    fn pages_follow_the_marker_and_the_limit() {
        let parts = layout(&[1, 1, 1, 1, 1, 1]);
        let first = parts_page(&parts, 3, 1);
        assert_eq!(first.total, 6);
        assert_eq!(first.parts.iter().map(|p| p.part_number).collect::<Vec<_>>(), vec![4]);
        assert_eq!(first.next_marker, Some(4));
        let rest = parts_page(&parts, 4, 10);
        assert_eq!(rest.parts.iter().map(|p| p.part_number).collect::<Vec<_>>(), vec![5, 6]);
        assert_eq!(rest.next_marker, None);
        let all = parts_page(&parts, 0, DEFAULT_MAX_PARTS);
        assert_eq!(all.parts.len(), 6);
        assert_eq!(all.next_marker, None);
    }

    #[test]
    fn a_page_past_the_end_is_empty_and_not_truncated() {
        let parts = layout(&[1, 1]);
        let page = parts_page(&parts, 5, 10);
        assert!(page.parts.is_empty());
        assert_eq!(page.next_marker, None);
        assert_eq!(page.total, 2);
    }

    #[test]
    fn an_exactly_full_last_page_is_not_truncated() {
        let parts = layout(&[1, 1, 1]);
        let page = parts_page(&parts, 1, 2);
        assert_eq!(page.parts.len(), 2);
        assert_eq!(page.next_marker, None);
    }
}
