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
        let mut c = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *c.per_tenant.entry((self.tenant, class)).or_insert(0) += 1;
        *c.totals.entry(class).or_insert(0) += 1;
    }
}

#[async_trait::async_trait]
impl<S: crate::BlobStore> crate::BlobStore for TenantView<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        self.inner.get_range(key, range).await
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.bill(OpClass::Read);
        self.inner.get_suffix(key, n).await
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
        self.inner.put(key, body).await
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.bill(OpClass::Write);
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.bill(OpClass::Delete);
        self.inner.delete_batch(keys).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.bill(OpClass::List);
        self.inner.list_unrestricted(prefix).await
    }
}
