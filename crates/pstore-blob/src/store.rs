//! The trait. Everything above this layer goes through it, so request accounting and
//! congestion control are unavoidable.

use crate::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome};
use bytes::Bytes;
use std::ops::Range;

/// The only way pstore touches durable storage.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// What this backend was **observed** to do.
    fn capabilities(&self) -> &Capabilities;

    /// Whole object.
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError>;

    /// One byte range.
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError>;

    /// Several byte ranges at once, **coalescing nearby ones into a single request**.
    ///
    /// The workhorse of the read path. Provided rather than required, so no backend can
    /// accidentally implement the naive one-request-per-range version.
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        let gap = self.capabilities().coalesce_gap;
        let mut out: Vec<Bytes> = vec![Bytes::new(); ranges.len()];
        for fetch in crate::coalesce(ranges, gap) {
            let base = fetch.span.start;
            let buf = self.get_range(key, fetch.span.clone()).await?;
            for i in fetch.serves {
                let Some(r) = ranges.get(i) else { continue };
                let (lo, hi) = ((r.start - base) as usize, (r.end - base) as usize);
                let slot = out
                    .get_mut(i)
                    .ok_or_else(|| BlobError::Other("range index out of bounds".to_owned()))?;
                // Slice, not copy: `Bytes` is refcounted, so the merged buffer is shared.
                *slot = buf.slice(lo..hi);
            }
        }
        Ok(out)
    }

    /// Object size without the body. Reserved for GC and repair — **never the hot path**,
    /// where the manifest already proves what exists.
    async fn head(&self, key: &Key) -> Result<u64, BlobError>;

    /// Atomic whole-object write.
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError>;

    /// Conditional write. The linearizable register the whole architecture rests on.
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError>;

    /// Batch delete, capped at [`Capabilities::max_batch_delete`].
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError>;

    /// ⚠️ **Enumeration. Permitted only in GC, disaster recovery, and admin tooling.**
    ///
    /// Named awkwardly on purpose: LIST is priced like a PUT, returns at most 1000 keys,
    /// is inherently serial, and tells you what objects exist rather than what is
    /// committed. Making it awkward to reach for is the cheapest enforcement available.
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError>;
}
