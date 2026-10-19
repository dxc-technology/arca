//! Delimiter grouping shared by ListObjects (V1 and V2) and ListObjectVersions.
//!
//! With a delimiter, the rows whose key has the delimiter after the prefix
//! roll up into one common prefix (`photos/2026/` for `photos/2026/a.jpg`
//! under prefix `photos/`), which counts once towards `max-keys`. The keys of
//! a common prefix are contiguous in key order, so once the listing reaches a
//! group the next query starts past every key under it (a skip-scan): a page
//! costs at most one query of [`LIST_BATCH`] rows per item it returns,
//! however many keys or versions each prefix holds. Without it a listing at
//! the root of a bucket read every row of the bucket.

use std::future::Future;

use arca_core::error::ArcaError;
use arca_core::types::ObjectRecord;

/// Rows per query. Small on purpose: the rows of a batch that follow the
/// first row of a group are read for nothing, so this bounds the waste of
/// each group, while a run of plain keys only costs one query per batch.
pub const LIST_BATCH: u32 = 100;

/// Where the next query of a delimited listing starts.
pub enum ListFrom {
    /// The first query: from the request's own marker.
    Start,
    /// Strictly after this row (its key; for versions, its position too).
    After(ObjectRecord),
    /// After every key under this common prefix.
    PastPrefix(String),
}

/// One result of a delimited listing, in key order.
pub enum Listed {
    Record(ObjectRecord),
    Prefix(String),
}

/// A page of a delimited listing.
pub struct DelimitedPage {
    pub items: Vec<Listed>,
    /// More results follow the last item.
    pub truncated: bool,
}

/// A key to resume a `key > cursor` listing at, past every key that starts
/// with `prefix`. No key sorts between it and the keys after the group except
/// keys under the group that continue with U+10FFFF: those are grouped again
/// and dropped, so the cursor is exact.
pub fn past_prefix(prefix: &str) -> String {
    format!("{prefix}\u{10ffff}")
}

/// The common prefix `key` rolls up into: the key up to and including the
/// first `delim` after `prefix`.
fn common_prefix(prefix: &str, delim: &str, key: &str) -> Option<String> {
    key[prefix.len()..]
        .find(delim)
        .map(|pos| key[..prefix.len() + pos + delim.len()].to_string())
}

/// Groups the rows `fetch` returns by `delim` into a page of at most
/// `max_keys` items. `fetch` lists up to [`LIST_BATCH`] rows under `prefix`
/// from where [`ListFrom`] says. Groups at or before `marker` were returned
/// by a previous page (a page that ends on a common prefix hands it out as
/// the marker) and are skipped.
pub async fn list_delimited<F, Fut>(
    prefix: &str,
    delim: &str,
    marker: Option<&str>,
    max_keys: u32,
    mut fetch: F,
) -> Result<DelimitedPage, ArcaError>
where
    F: FnMut(ListFrom) -> Fut,
    Fut: Future<Output = Result<Vec<ObjectRecord>, ArcaError>>,
{
    let mut page = DelimitedPage { items: Vec::new(), truncated: false };
    let mut from = ListFrom::Start;
    loop {
        let records = fetch(from).await?;
        let exhausted = (records.len() as u32) < LIST_BATCH;
        let mut last_group = None;
        for record in &records {
            let group = common_prefix(prefix, delim, &record.key);
            last_group = group.clone();
            if let Some(cp) = &group {
                let returned = marker.is_some_and(|m| cp.as_str() <= m);
                let current = matches!(page.items.last(), Some(Listed::Prefix(p)) if p == cp);
                if returned || current {
                    continue;
                }
            }
            if page.items.len() as u32 >= max_keys {
                page.truncated = true;
                return Ok(page);
            }
            page.items.push(match group {
                Some(cp) => Listed::Prefix(cp),
                None => Listed::Record(record.clone()),
            });
        }
        let Some(last) = records.last() else { return Ok(page) };
        if exhausted {
            return Ok(page);
        }
        from = match last_group {
            Some(cp) => ListFrom::PastPrefix(cp),
            None => ListFrom::After(last.clone()),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn record(key: &str) -> ObjectRecord {
        ObjectRecord {
            bucket: "b".to_string(),
            key: key.to_string(),
            blob_id: arca_core::types::BlobId(String::new()),
            size: 0,
            etag: String::new(),
            content_type: None,
            last_modified: chrono::Utc::now(),
            metadata: Default::default(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            parts: None,
            replication_status: None,
            lock_updated_at: None,
            content_updated_at: None,
        }
    }

    /// A sorted key set behind a `key > cursor` listing, counting the
    /// queries and the rows it reads.
    struct Keys {
        keys: Vec<String>,
        queries: Cell<usize>,
        rows: Cell<usize>,
    }

    impl Keys {
        fn new(mut keys: Vec<String>) -> Self {
            keys.sort();
            Keys { keys, queries: Cell::new(0), rows: Cell::new(0) }
        }

        fn after(&self, prefix: &str, cursor: Option<&str>) -> Vec<ObjectRecord> {
            self.queries.set(self.queries.get() + 1);
            let out: Vec<ObjectRecord> = self
                .keys
                .iter()
                .filter(|k| k.starts_with(prefix) && cursor.is_none_or(|c| k.as_str() > c))
                .take(LIST_BATCH as usize)
                .map(|k| record(k))
                .collect();
            self.rows.set(self.rows.get() + out.len());
            out
        }

        async fn page(&self, prefix: &str, marker: Option<&str>, max_keys: u32) -> DelimitedPage {
            list_delimited(prefix, "/", marker, max_keys, |from| {
                let cursor = match from {
                    ListFrom::Start => marker.map(str::to_string),
                    ListFrom::After(r) => Some(r.key),
                    ListFrom::PastPrefix(p) => Some(past_prefix(&p)),
                };
                let rows = self.after(prefix, cursor.as_deref());
                async move { Ok(rows) }
            })
            .await
            .unwrap()
        }

        /// Every page, resuming after the last item as S3 clients do.
        async fn walk(&self, prefix: &str, max_keys: u32) -> Vec<String> {
            let mut out = Vec::new();
            let mut marker: Option<String> = None;
            loop {
                let page = self.page(prefix, marker.as_deref(), max_keys).await;
                let names: Vec<String> = page.items.iter().map(name).collect();
                out.extend(names.iter().cloned());
                if !page.truncated {
                    return out;
                }
                marker = names.last().cloned();
            }
        }
    }

    fn name(item: &Listed) -> String {
        match item {
            Listed::Record(r) => r.key.clone(),
            Listed::Prefix(p) => p.clone(),
        }
    }

    /// The grouping computed naively, over every key.
    fn expected(keys: &[String], prefix: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut sorted = keys.to_vec();
        sorted.sort();
        for key in sorted.iter().filter(|k| k.starts_with(prefix)) {
            let item = common_prefix(prefix, "/", key).unwrap_or_else(|| key.clone());
            if out.last() != Some(&item) {
                out.push(item);
            }
        }
        out
    }

    fn tree(groups: usize, per_group: usize) -> Vec<String> {
        let mut keys = Vec::new();
        for g in 0..groups {
            for k in 0..per_group {
                keys.push(format!("d{g:02}/k{k:05}"));
            }
            keys.push(format!("d{g:02}-plain"));
        }
        keys
    }

    #[test]
    fn past_prefix_sorts_after_every_key_of_the_group() {
        let cursor = past_prefix("a/");
        assert!("a/zzzz" < cursor.as_str());
        assert!("a/\u{fffd}" < cursor.as_str());
        assert!(cursor.as_str() < "a0");
        assert!(cursor.as_str() < "b");
    }

    #[tokio::test]
    async fn pages_match_the_naive_grouping_at_every_page_size() {
        let mut keys = tree(5, 150);
        keys.extend(["a", "a/", "a//x", "z/\u{10ffff}/deep", "z/\u{10ffff}"].map(String::from));
        let store = Keys::new(keys.clone());
        for prefix in ["", "d02/", "z/"] {
            for max_keys in [1, 2, 3, 7, 1000] {
                assert_eq!(
                    store.walk(prefix, max_keys).await,
                    expected(&keys, prefix),
                    "prefix {prefix:?}, max_keys {max_keys}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_group_costs_one_query_whatever_its_size() {
        let store = Keys::new(tree(10, 10_000));
        let page = store.page("", None, 1000).await;
        assert_eq!(page.items.len(), 20);
        assert!(!page.truncated);
        // Without the skip-scan: 100,010 rows in 1,001 queries.
        assert!(store.queries.get() <= 21, "{} queries", store.queries.get());
        assert!(store.rows.get() <= 21 * LIST_BATCH as usize, "{} rows", store.rows.get());
    }

    #[tokio::test]
    async fn a_marker_on_a_common_prefix_skips_the_whole_group() {
        let store = Keys::new(tree(3, 500));
        let page = store.page("", Some("d00/"), 2).await;
        let names: Vec<String> = page.items.iter().map(name).collect();
        // `-` sorts before `/`: d00-plain precedes the d00/ group.
        assert_eq!(names, ["d01-plain", "d01/"]);
        assert!(page.truncated);
        assert!(store.queries.get() <= 3, "{} queries", store.queries.get());
    }

    #[tokio::test]
    async fn a_full_page_is_truncated_only_when_more_follows() {
        let store = Keys::new(tree(2, 3));
        let exact = store.page("", None, 4).await;
        assert_eq!(exact.items.len(), 4);
        assert!(!exact.truncated);
        let short = store.page("", None, 3).await;
        assert!(short.truncated);
    }
}
