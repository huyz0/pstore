//! The trait. Everything above this layer goes through it, so request accounting and
//! congestion control are unavoidable.

use crate::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome};
use bytes::Bytes;
use pstore_types::CasTag;
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
        let plan = crate::coalesce(ranges, gap);
        // ⚠️ Issued together, not in a loop. Coalescing merges what is NEARBY; ranges far
        // apart legitimately stay separate fetches, and awaiting each before issuing the
        // next would make a wide scan cost one round trip per block -- 1.2 s for forty
        // blocks at 30 ms a hop, and invisible to every functional test because the
        // answer is identical. Width is free; depth is not.
        let fetched = futures_util::future::try_join_all(
            plan.iter().map(|f| self.get_range(key, f.span.clone())),
        )
        .await?;

        let mut out: Vec<(usize, Bytes)> = Vec::with_capacity(ranges.len());
        for (fetch, buf) in plan.into_iter().zip(fetched) {
            let base = fetch.span.start;
            for (i, r) in fetch.serves {
                let (lo, hi) = ((r.start - base) as usize, (r.end - base) as usize);
                // Slice, not copy: `Bytes` is refcounted, so the merged buffer is shared
                // by every range it serves.
                out.push((i, buf.slice(lo..hi)));
            }
        }
        out.sort_by_key(|(i, _)| *i);
        Ok(out.into_iter().map(|(_, b)| b).collect())
    }

    /// The last `n` bytes, without knowing the object's length.
    ///
    /// `Range: bytes=-N`, supported by S3, GCS and Azure alike. **This is what makes
    /// Pattern 6 work**: a self-describing footer can be read from the key alone, with no
    /// prior `head` — and `head` is billed as a read, so requiring one would double the
    /// cost of every cold open and put a HEAD on the hot path the design forbids.
    ///
    /// Returns fewer than `n` bytes only when the object is shorter than `n`.
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError>;

    /// The object **and** the tag it had when read, in one operation.
    ///
    /// ⚠️ Not `get` followed by `get_tag`. Between two calls another writer can land, so
    /// the caller would hold old bytes with a new tag — and its CAS would then succeed,
    /// silently overwriting a commit it never saw. A lost update, and the reason this is
    /// a required method rather than a convenience.
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError>;

    /// The current CAS tag, or `None` if the object is absent.
    ///
    /// The rebase step of the commit protocol: read the state an attempt will be
    /// conditioned on. Provided in terms of `head`-like access so every backend has it.
    async fn get_tag(&self, key: &Key) -> Option<pstore_types::CasTag>;

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
