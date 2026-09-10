//! The decorator that refuses before it dispatches.

use crate::{Meter, Resource};
use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, OpClass, Precondition, PutOutcome,
};
use pstore_types::{CasTag, TenantId};
use std::sync::Arc;
use std::time::Duration;

/// Meters and refuses one tenant's traffic to `inner`.
///
/// ⚠️ **Outermost, above `Accounted`.** `BlobStore` is the only way pstore touches durable
/// storage, so it is the only place a request cannot escape the meter. Enforcing in the engine
/// would leave the catalog, the roster and every future caller unmetered.
pub struct Metered<S> {
    inner: Arc<S>,
    meter: Arc<Meter>,
    tenant: TenantId,
    /// The meter's clock, supplied rather than read. See [`crate::Bucket`].
    now: Arc<dyn Fn() -> Duration + Send + Sync>,
}

impl<S> std::fmt::Debug for Metered<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The clock is a closure and has no `Debug`; the tenant is what a reader wants.
        f.debug_struct("Metered")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl<S: BlobStore> Metered<S> {
    /// Meters `inner` for `tenant`, taking the time from `now`.
    pub fn new(
        inner: Arc<S>,
        meter: Arc<Meter>,
        tenant: TenantId,
        now: Arc<dyn Fn() -> Duration + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            meter,
            tenant,
            now,
        }
    }

    fn refused(&self, what: Resource) -> String {
        // ⚠️ Names the tenant AND the resource. "Quota exceeded" leaves an operator with
        // nothing to do, and this message is the only thing a refused caller receives.
        format!(
            "429 quota exceeded for tenant {}: {what} - retry after the bucket refills",
            self.tenant.0
        )
    }

    /// Reserves for one operation of `n` requests, or explains why not.
    ///
    /// ⚠️ **Bytes are checked FIRST, and the order is the whole of "the buckets are
    /// independent".** Reserving requests before discovering the byte quota is exhausted
    /// spends request tokens on an operation that never issues a request — so a client
    /// retrying against a byte quota drains its own request bucket to zero and is then
    /// refused for the wrong resource. Checking credit takes nothing, so it is free to do it
    /// first.
    fn admit(&self, n: u64) -> Result<Duration, BlobError> {
        let now = (self.now)();
        if !self.meter.bytes_in_credit(self.tenant, now) {
            return Err(BlobError::Other(self.refused(Resource::Bytes)));
        }
        if !self.meter.reserve_requests(self.tenant, n, now) {
            return Err(BlobError::Other(self.refused(Resource::Requests)));
        }
        Ok(now)
    }

    fn settle(&self, class: OpClass, requests: u64, bytes: u64, now: Duration) {
        self.meter.settle(self.tenant, class, requests, bytes, now);
    }

    /// Runs a fan-out, billing **the bytes the merged fetches actually moved**.
    ///
    /// ⚠️ **Not the sum of the slices handed back, and the difference is the coalescer's
    /// whole point.** The trait's default returns each requested range sliced out of a merged
    /// buffer, so summing what the caller receives misses the gap bytes that crossed the wire
    /// — and `Accounted` beneath bills the merged buffer, because the default dispatches one
    /// `get_range` per *fetch*. Measured on an 8-range fan-out at a 1 KiB gap: 64 bytes
    /// against 456. A tenant issuing merged fan-outs — the read path's normal shape — would
    /// be billed a fraction of what it moved, and the byte bucket would bound nothing on
    /// exactly the reads it exists for.
    ///
    /// ⚠️ It also coalesces **once**. Planning in one place and dispatching in another left
    /// two independent calls to `coalesce` that agreed only by inspection.
    async fn fan_out(
        &self,
        key: &Key,
        ranges: &[std::ops::Range<u64>],
        class: Option<Class>,
    ) -> Result<Vec<Bytes>, BlobError> {
        let plan = pstore_blob::coalesce(ranges, self.inner.capabilities().coalesce_gap);
        let now = self.admit(plan.len() as u64)?;

        // ⚠️ `self.inner`, never `self`. Dispatching through our own `get_range_as` would
        // re-enter the meter and charge every fetch twice.
        let fetched = futures_util::future::try_join_all(plan.iter().map(|f| async {
            match class {
                Some(c) => self.inner.get_range_as(key, f.span.clone(), c).await,
                None => self.inner.get_range(key, f.span.clone()).await,
            }
        }))
        .await;

        let fetched = match fetched {
            Ok(f) => f,
            Err(e) => {
                // Nothing to debit: `try_join_all` gives us no successful buffers to bill,
                // which is the error-path gap criterion 8 is scoped around.
                self.settle(OpClass::Read, plan.len() as u64, 0, now);
                return Err(e);
            }
        };
        let moved: u64 = fetched.iter().map(|b| b.len() as u64).sum();
        self.settle(OpClass::Read, plan.len() as u64, moved, now);

        // The trait default's own slicing, so a caller cannot tell the meter is there.
        let mut out: Vec<(usize, Bytes)> = Vec::with_capacity(ranges.len());
        for (fetch, buf) in plan.into_iter().zip(fetched) {
            let base = fetch.span.start;
            for (i, r) in fetch.serves {
                let (lo, hi) = (
                    usize::try_from(r.start - base).unwrap_or(usize::MAX),
                    usize::try_from(r.end - base).unwrap_or(usize::MAX),
                );
                let Some(slice) = buf.get(lo..hi) else {
                    return Err(BlobError::Other(format!(
                        "coalesced fetch for {key} returned {} bytes, short of {hi}",
                        buf.len()
                    )));
                };
                out.push((i, Bytes::copy_from_slice(slice)));
            }
        }
        out.sort_by_key(|(i, _)| *i);
        Ok(out.into_iter().map(|(_, b)| b).collect())
    }
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for Metered<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.get(key).await;
        self.settle(
            OpClass::Read,
            1,
            out.as_ref().map_or(0, |b| b.len() as u64),
            now,
        );
        out
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.get_range(key, range).await;
        self.settle(
            OpClass::Read,
            1,
            out.as_ref().map_or(0, |b| b.len() as u64),
            now,
        );
        out
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.get_suffix(key, n).await;
        self.settle(
            OpClass::Read,
            1,
            out.as_ref().map_or(0, |b| b.len() as u64),
            now,
        );
        out
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.get_with_tag(key).await;
        self.settle(
            OpClass::Read,
            1,
            out.as_ref().map_or(0, |(b, _)| b.len() as u64),
            now,
        );
        out
    }

    /// ⚠️ **Metered and never refused.**
    ///
    /// It returns `Option<CasTag>` with no error channel, so a refusal could only be `None` —
    /// which means "the object is absent" to the caller. That is the **rebase step of the CAS
    /// commit protocol**: a quota refusal would become a `Precondition::NotExists` write
    /// against an object that exists, which is a silent wrong answer rather than a refusal.
    /// A hole, named rather than papered over.
    async fn get_tag(&self, key: &Key) -> Option<CasTag> {
        let now = (self.now)();
        let out = self.inner.get_tag(key).await;
        self.settle(OpClass::Read, 1, 0, now);
        out
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.head(key).await;
        // ⚠️ A `head` moves no body, and `Accounted` bills it no bytes. Charging the object's
        // size would make a metadata probe look like a full fetch — and would put criterion 8
        // permanently out of reach.
        self.settle(OpClass::Read, 1, 0, now);
        out
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        let now = self.admit(1)?;
        let n = body.len() as u64;
        let out = self.inner.put(key, body).await;
        self.settle(OpClass::Write, 1, if out.is_ok() { n } else { 0 }, now);
        out
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        // ⚠️ `CasError::Io`, never `Contended`. Every caller in the tree treats `Contended` as
        // "retry the same attempt unchanged", which would turn one quota refusal into a retry
        // storm against the quota. `Io` is fatal to the caller, which is what a refusal is.
        let now = (self.now)();
        // Bytes first, for the reason `admit` gives.
        if !self.meter.bytes_in_credit(self.tenant, now) {
            return Err(CasError::Io(self.refused(Resource::Bytes)));
        }
        if !self.meter.reserve_requests(self.tenant, 1, now) {
            return Err(CasError::Io(self.refused(Resource::Requests)));
        }
        let n = body.len() as u64;
        let out = self.inner.put_conditional(key, body, pre).await;
        self.settle(OpClass::Write, 1, if out.is_ok() { n } else { 0 }, now);
        out
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.delete_batch(keys).await;
        self.settle(OpClass::Delete, 1, 0, now);
        out
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.list_unrestricted(prefix).await;
        self.settle(OpClass::List, 1, 0, now);
        out
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.get_range_as(key, range, class).await;
        self.settle(
            OpClass::Read,
            1,
            out.as_ref().map_or(0, |b| b.len() as u64),
            now,
        );
        out
    }

    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.get_suffix_as(key, n, class).await;
        self.settle(
            OpClass::Read,
            1,
            out.as_ref().map_or(0, |b| b.len() as u64),
            now,
        );
        out
    }

    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        let now = self.admit(1)?;
        let out = self.inner.get_immutable(key, class).await;
        self.settle(
            OpClass::Read,
            1,
            out.as_ref().map_or(0, |b| b.len() as u64),
            now,
        );
        out
    }

    /// ⚠️ **Overridden, and this is the larger of the two fan-outs.** The dense read path uses
    /// the *unhinted* `get_ranges` — `pstore-format`'s reader and `pstore-index`'s vector
    /// index — while only text, sparse and the cache use `get_ranges_as`. Inheriting the
    /// default here would dispatch through `self.get_range` and reserve **per range inside the
    /// loop**, which is exactly what a whole-operation reservation exists to prevent, left
    /// alive on the path with four thousand ranges.
    async fn get_ranges(
        &self,
        key: &Key,
        ranges: &[std::ops::Range<u64>],
    ) -> Result<Vec<Bytes>, BlobError> {
        self.fan_out(key, ranges, None).await
    }

    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[std::ops::Range<u64>],
        class: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.fan_out(key, ranges, Some(class)).await
    }
}
