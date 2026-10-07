//! Reading composite blobs: the parts of a multipart object, streamed in order.
//!
//! Shared by `FsBlobStore` and `EncryptingBlobStore`. A composite can have up
//! to 10,000 parts, so a read must never hold one open file per part: the
//! parts are opened one at a time, each when the stream before it ends.

use std::future::Future;
use std::io;

use futures_util::{StreamExt, TryStreamExt};

use arca_core::error::ArcaError;
use arca_core::store::{ByteRange, ByteStream, CompositePart};

/// A part that a composite read touches, and the sub-range of it to return
/// (`None` for the whole part).
#[derive(Debug, Clone)]
pub(crate) struct PartRead {
    pub part: CompositePart,
    pub range: Option<ByteRange>,
}

/// Resolves a read of `range` (`None` for the whole blob) over the
/// concatenation of `parts`: the parts it touches, in order, each with its
/// sub-range, and the number of bytes the read returns.
pub(crate) fn plan_composite_read(
    parts: &[CompositePart],
    range: Option<ByteRange>,
) -> (Vec<PartRead>, u64) {
    let total: u64 = parts.iter().map(|p| p.plaintext_size).sum();
    // Absolute range [range_start, range_end], inclusive.
    let (range_start, range_end) = match range {
        Some(r) => {
            let end = r.end.unwrap_or_else(|| total.saturating_sub(1));
            (r.start, end.min(total.saturating_sub(1)))
        }
        None => (0, total.saturating_sub(1)),
    };
    let content_length = if total == 0 {
        0
    } else if range_end >= range_start {
        range_end - range_start + 1
    } else {
        0
    };

    let mut reads = Vec::new();
    let mut cum_start: u64 = 0;
    for part in parts {
        let part_size = part.plaintext_size;
        let part_end_excl = cum_start + part_size;
        // Skip parts entirely outside the requested range.
        if part_size == 0 || content_length == 0 || part_end_excl <= range_start || cum_start > range_end {
            cum_start = part_end_excl;
            continue;
        }
        let sub_start = range_start.saturating_sub(cum_start);
        let sub_end = (range_end - cum_start).min(part_size - 1);
        let range = if sub_start == 0 && sub_end == part_size - 1 {
            None
        } else {
            Some(ByteRange { start: sub_start, end: Some(sub_end) })
        };
        reads.push(PartRead { part: part.clone(), range });
        cum_start = part_end_excl;
    }
    (reads, content_length)
}

/// Concatenates the streams that `open` returns for `items`, opening each item
/// only when the stream before it has ended, so at most one is open at a time.
///
/// The first item is opened before this returns, so a failure there (typically
/// a missing part) is still an `Err` the caller can turn into an error
/// response. A later item that fails to open ends the stream with an error,
/// which aborts a response already in progress.
pub(crate) async fn concat_lazily<T, F, Fut>(items: Vec<T>, mut open: F) -> Result<ByteStream, ArcaError>
where
    T: Send + 'static,
    F: FnMut(T) -> Fut + Send + 'static,
    Fut: Future<Output = Result<ByteStream, ArcaError>> + Send + 'static,
{
    let mut items = items.into_iter();
    let Some(first) = items.next() else {
        return Ok(Box::pin(futures_util::stream::empty()));
    };
    let first = open(first).await?;
    let rest = futures_util::stream::iter(items)
        .then(open)
        .map_err(io::Error::other)
        .try_flatten();
    Ok(Box::pin(first.chain(rest)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use arca_core::types::BlobId;
    use bytes::Bytes;

    fn parts(sizes: &[u64]) -> Vec<CompositePart> {
        sizes
            .iter()
            .map(|&s| CompositePart {
                blob_id: BlobId::new(),
                plaintext_size: s,
                plaintext_etag: String::new(),
                encryption: None,
            })
            .collect()
    }

    /// (index of the part, sub-range) for each part a read touches.
    fn summary(parts: &[CompositePart], reads: &[PartRead]) -> Vec<(usize, Option<(u64, Option<u64>)>)> {
        reads
            .iter()
            .map(|r| {
                let i = parts.iter().position(|p| p.blob_id == r.part.blob_id).unwrap();
                (i, r.range.map(|r| (r.start, r.end)))
            })
            .collect()
    }

    fn range(start: u64, end: Option<u64>) -> Option<ByteRange> {
        Some(ByteRange { start, end })
    }

    #[test]
    fn full_read_touches_every_part_whole() {
        let p = parts(&[100, 100, 50]);
        let (reads, len) = plan_composite_read(&p, None);
        assert_eq!(len, 250);
        assert_eq!(summary(&p, &reads), vec![(0, None), (1, None), (2, None)]);
    }

    #[test]
    fn range_within_one_part() {
        let p = parts(&[100, 100, 100]);
        let (reads, len) = plan_composite_read(&p, range(110, Some(130)));
        assert_eq!(len, 21);
        assert_eq!(summary(&p, &reads), vec![(1, Some((10, Some(30))))]);
    }

    #[test]
    fn range_across_three_parts() {
        let p = parts(&[100, 100, 100, 100]);
        let (reads, len) = plan_composite_read(&p, range(150, Some(349)));
        assert_eq!(len, 200);
        assert_eq!(
            summary(&p, &reads),
            vec![(1, Some((50, Some(99)))), (2, None), (3, Some((0, Some(49))))]
        );
    }

    #[test]
    fn range_on_part_boundaries_reads_whole_parts() {
        let p = parts(&[100, 100, 100]);
        let (reads, len) = plan_composite_read(&p, range(100, Some(199)));
        assert_eq!(len, 100);
        assert_eq!(summary(&p, &reads), vec![(1, None)]);
    }

    #[test]
    fn open_ended_range_and_end_past_the_blob_are_clamped() {
        let p = parts(&[100, 100]);
        let (reads, len) = plan_composite_read(&p, range(150, None));
        assert_eq!(len, 50);
        assert_eq!(summary(&p, &reads), vec![(1, Some((50, Some(99))))]);
        let (reads, len) = plan_composite_read(&p, range(50, Some(10_000)));
        assert_eq!(len, 150);
        assert_eq!(summary(&p, &reads), vec![(0, Some((50, Some(99)))), (1, None)]);
    }

    #[test]
    fn empty_parts_are_skipped() {
        let p = parts(&[0, 100, 0, 100]);
        let (reads, len) = plan_composite_read(&p, None);
        assert_eq!(len, 200);
        assert_eq!(summary(&p, &reads), vec![(1, None), (3, None)]);
    }

    #[test]
    fn empty_and_unsatisfiable_reads_touch_nothing() {
        let (reads, len) = plan_composite_read(&parts(&[]), None);
        assert_eq!((reads.len(), len), (0, 0));
        let (reads, len) = plan_composite_read(&parts(&[0, 0]), None);
        assert_eq!((reads.len(), len), (0, 0));
        let (reads, len) = plan_composite_read(&parts(&[100]), range(200, Some(300)));
        assert_eq!((reads.len(), len), (0, 0));
    }

    fn bytes(data: &'static [u8]) -> ByteStream {
        Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from_static(data))]))
    }

    async fn collect(stream: ByteStream) -> Result<Vec<u8>, io::Error> {
        let mut stream = std::pin::pin!(stream);
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    /// A stream that counts itself as open until it is dropped.
    struct Tracked {
        inner: ByteStream,
        open: Arc<AtomicUsize>,
    }

    impl futures_core::Stream for Tracked {
        type Item = Result<Bytes, io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.inner.as_mut().poll_next(cx)
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.open.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn opens_one_item_at_a_time_in_order() {
        let open = Arc::new(AtomicUsize::new(0));
        let max_open = Arc::new(AtomicUsize::new(0));
        let opened = Arc::new(std::sync::Mutex::new(Vec::new()));
        let items: Vec<&'static [u8]> = vec![b"ab", b"cd", b"ef", b"gh"];
        let (o, m, log) = (open.clone(), max_open.clone(), opened.clone());
        let stream = concat_lazily(items, move |data: &'static [u8]| {
            let (o, m, log) = (o.clone(), m.clone(), log.clone());
            async move {
                let now = o.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(now, Ordering::SeqCst);
                log.lock().unwrap().push(data);
                Ok(Box::pin(Tracked { inner: bytes(data), open: o }) as ByteStream)
            }
        })
        .await
        .unwrap();
        assert_eq!(*opened.lock().unwrap(), vec![b"ab" as &[u8]], "only the first item is opened up front");

        assert_eq!(collect(stream).await.unwrap(), b"abcdefgh");
        assert_eq!(opened.lock().unwrap().len(), 4);
        assert_eq!(max_open.load(Ordering::SeqCst), 1, "an item is opened only once the one before it is dropped");
        assert_eq!(open.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn first_item_failure_is_returned_before_streaming() {
        let result = concat_lazily(vec![1, 2], |i| async move {
            if i == 1 {
                Err(ArcaError::Internal("open part blob: gone".into()))
            } else {
                Ok(bytes(b"x"))
            }
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn later_item_failure_ends_the_stream_with_an_error() {
        let stream = concat_lazily(vec![1, 2, 3], |i| async move {
            match i {
                2 => Err(ArcaError::Internal("open part blob: gone".into())),
                _ => Ok(bytes(b"x")),
            }
        })
        .await
        .unwrap();
        let mut stream = std::pin::pin!(stream);
        assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(b"x"));
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(err.to_string().contains("open part blob: gone"), "{err}");
    }

    #[tokio::test]
    async fn no_items_is_an_empty_stream() {
        let stream = concat_lazily(Vec::<u8>::new(), |_| async { Ok(bytes(b"x")) }).await.unwrap();
        assert!(collect(stream).await.unwrap().is_empty());
    }
}
