//! Per-tenant, per-class request accounting.
//!
//! Every design document in this project states an `RA` budget — requests per operation,
//! split by class. This is what turns those from claims into assertions, and it is the
//! same counter that attributes cost to a tenant in production (Design rule 13).

use crate::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome};
use bytes::Bytes;
use pstore_types::TenantId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// What a blob request is billed as.
///
/// The split is the pricing structure, not ours: on S3 a PUT costs 12.5 GETs, and **LIST
/// is billed at the write rate** while returning at most 1000 keys. Folding LIST into
/// reads would hide the one operation this architecture exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpClass {
    /// PUT, COPY, POST — and LIST is priced here too, but counted separately.
    Write,
    /// GET, HEAD, SELECT.
    Read,
    /// LIST. Priced as a write; counted apart so it can be asserted at zero.
    List,
    /// DELETE. Free on S3, billed on GCS and Azure.
    Delete,
}

#[derive(Debug, Default)]
struct Counters {
    per_tenant: HashMap<(TenantId, OpClass), u64>,
    totals: HashMap<OpClass, u64>,
    /// Bytes, which a request count cannot stand in for.
    ///
    /// ⚠️ "Probe 64 posting lists instead of 8" is one request either way and eight times
    /// the bytes — that is the whole point of the round-trip architecture, and it means a
    /// recall number bought with unbounded bandwidth is indistinguishable from an
    /// efficient one through a request counter alone.
    bytes_per_tenant: HashMap<(TenantId, OpClass), u64>,
    /// What was asked for, when recording is on.
    ///
    /// The store cannot know what a byte range *means* — sections are the format's idea —
    /// so it records the request and lets the caller attribute it using the same footer
    /// the reader used. That keeps the accounting honest without teaching the blob layer
    /// about segments.
    ranges: Option<Vec<(Key, std::ops::Range<u64>)>>,
}

/// Wraps a [`BlobStore`](crate::BlobStore) and counts what passes through it.
#[derive(Debug, Clone)]
pub struct Accounted<S> {
    inner: Arc<S>,
    counters: Arc<Mutex<Counters>>,
}

impl<S: crate::BlobStore> Accounted<S> {
    /// Wraps a backend.
    pub fn new(inner: S) -> Self {
        Self {
            inner: Arc::new(inner),
            counters: Arc::new(Mutex::new(Counters::default())),
        }
    }

    /// A handle that attributes everything it does to one tenant.
    #[must_use]
    pub fn as_tenant(&self, tenant: TenantId) -> TenantView<S> {
        TenantView {
            inner: Arc::clone(&self.inner),
            counters: Arc::clone(&self.counters),
            tenant,
        }
    }

    /// Requests of `class` billed to `tenant`.
    #[must_use]
    pub fn count(&self, tenant: TenantId, class: OpClass) -> u64 {
        self.lock()
            .per_tenant
            .get(&(tenant, class))
            .copied()
            .unwrap_or(0)
    }

    /// Bytes of `class` billed to `tenant`.
    ///
    /// A `head` moves no body and adds nothing here: counting it as the object's size
    /// would make a metadata probe look like a full fetch and hide the exact difference
    /// the design turns on.
    #[must_use]
    pub fn bytes(&self, tenant: TenantId, class: OpClass) -> u64 {
        self.lock()
            .bytes_per_tenant
            .get(&(tenant, class))
            .copied()
            .unwrap_or(0)
    }

    /// Starts recording the byte ranges of every read.
    ///
    /// Off by default: a long-running store would grow this without bound, and it exists
    /// for assertions rather than for production accounting.
    pub fn record_ranges(&self) {
        self.lock().ranges.get_or_insert_with(Vec::new).clear();
    }

    /// The ranges recorded since [`Self::record_ranges`], in request order.
    #[must_use]
    pub fn ranges(&self) -> Vec<(Key, std::ops::Range<u64>)> {
        self.lock().ranges.clone().unwrap_or_default()
    }

    /// Bytes read from `key` that fall inside `span`.
    ///
    /// The section assertion: given a section's byte range from the footer, this is how
    /// many bytes of it a query actually moved. Overlap, not containment — a coalesced
    /// fetch spanning two sections is billed to both, which is the honest answer because
    /// the bytes really did cross the wire.
    #[must_use]
    pub fn bytes_in(&self, key: &Key, span: std::ops::Range<u64>) -> u64 {
        self.lock()
            .ranges
            .iter()
            .flatten()
            .filter(|(k, _)| k == key)
            .map(|(_, r)| r.end.min(span.end).saturating_sub(r.start.max(span.start)))
            .sum()
    }

    /// Requests of `class` across every tenant.
    #[must_use]
    pub fn total(&self, class: OpClass) -> u64 {
        self.lock().totals.get(&class).copied().unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One tenant's view of an [`Accounted`] store.
#[derive(Debug, Clone)]
pub struct TenantView<S> {
    inner: Arc<S>,
    counters: Arc<Mutex<Counters>>,
    tenant: TenantId,
}

impl<S> TenantView<S> {
    /// Billed **before** the call, so a failure is counted too: AWS charges for failed
    /// conditional requests, which is exactly what makes a CAS retry storm expensive.
    fn bill(&self, class: OpClass) {
        let mut c = self.lock();
        *c.per_tenant.entry((self.tenant, class)).or_insert(0) += 1;
        *c.totals.entry(class).or_insert(0) += 1;
    }

    /// Bills bytes that actually crossed the wire.
    ///
    /// Separate from [`Self::bill`] because the two differ on failure: a refused request is
    /// still billed as a request — AWS charges for failed conditional writes, which is what
    /// makes a CAS storm expensive — but it moves no bytes.
    fn bill_bytes(&self, class: OpClass, n: u64) {
        *self
            .lock()
            .bytes_per_tenant
            .entry((self.tenant, class))
            .or_insert(0) += n;
    }

    /// Records a read's span, if recording is on.
    fn note_range(&self, key: &Key, range: std::ops::Range<u64>) {
        if let Some(log) = self.lock().ranges.as_mut() {
            log.push((key.clone(), range));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl<S: crate::BlobStore> crate::BlobStore for TenantView<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        let out = self.inner.get(key).await;
        if let Ok(b) = &out {
            self.bill_bytes(OpClass::Read, b.len() as u64);
            self.note_range(key, 0..b.len() as u64);
        }
        out
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        let out = self.inner.get_range(key, range.clone()).await;
        if let Ok(b) = &out {
            self.bill_bytes(OpClass::Read, b.len() as u64);
            self.note_range(key, range);
        }
        out
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, pstore_types::CasTag), BlobError> {
        self.bill(OpClass::Read);
        let out = self.inner.get_with_tag(key).await;
        if let Ok((b, _)) = &out {
            self.bill_bytes(OpClass::Read, b.len() as u64);
            self.note_range(key, 0..b.len() as u64);
        }
        out
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        let out = self.inner.get_suffix(key, n).await;
        if let Ok(b) = &out {
            let len = b.len() as u64;
            self.bill_bytes(OpClass::Read, len);
            // ⚠️ Resolved to the absolute range it actually moved. A suffix is the one
            // read whose span is not known until it returns, and the footer is ALWAYS read
            // as a suffix -- so leaving it unresolved would make the one section every
            // query touches the one section nothing can attribute.
            let end = self.inner.head(key).await.unwrap_or(len);
            self.note_range(key, end.saturating_sub(len)..end);
        }
        out
    }

    async fn get_tag(&self, key: &Key) -> Option<pstore_types::CasTag> {
        self.bill(OpClass::Read);
        self.inner.get_tag(key).await
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.bill(OpClass::Read);
        self.inner.head(key).await
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.bill(OpClass::Write);
        let n = body.len() as u64;
        let out = self.inner.put(key, body).await;
        // ⚠️ Billed only on success, unlike the request itself. A refused write is charged
        // as a request -- AWS bills failed conditional writes, which is what makes a CAS
        // storm expensive -- but no bytes reached the store, and pretending they did would
        // make a retry loop look like a bandwidth problem.
        if out.is_ok() {
            self.bill_bytes(OpClass::Write, n);
        }
        out
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.bill(OpClass::Write);
        let n = body.len() as u64;
        let out = self.inner.put_conditional(key, body, pre).await;
        if out.is_ok() {
            self.bill_bytes(OpClass::Write, n);
        }
        out
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.bill(OpClass::Delete);
        self.inner.delete_batch(keys).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.bill(OpClass::List);
        self.inner.list_unrestricted(prefix).await
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: crate::Class,
    ) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        let out = self.inner.get_range_as(key, range.clone(), class).await;
        if let Ok(b) = &out {
            self.bill_bytes(OpClass::Read, b.len() as u64);
            self.note_range(key, range);
        }
        out
    }

    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: crate::Class,
    ) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        let out = self.inner.get_suffix_as(key, n, class).await;
        if let Ok(b) = &out {
            self.bill_bytes(OpClass::Read, b.len() as u64);
        }
        out
    }

    async fn get_immutable(&self, key: &Key, class: crate::Class) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        let out = self.inner.get_immutable(key, class).await;
        if let Ok(b) = &out {
            self.bill_bytes(OpClass::Read, b.len() as u64);
        }
        out
    }
}
