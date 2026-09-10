//! Recording a tenant, at tenant-lifecycle rate rather than at commit rate.

use crate::bucket::{Publish, merge, publish, read_head, write_head};
use crate::keys::{Width, bucket_of};
use crate::record::TenantRecord;
use crate::{CatalogError, MAX_CAS_ATTEMPTS, MAX_PENDING};
use pstore_blob::BlobStore;
use pstore_types::TenantId;
use std::collections::HashMap;
use std::sync::Mutex;

/// Records tenants into the catalog.
///
/// ⚠️ **The rate is the design.** C-12 buys a bounded write with a CAS on the append path,
/// and that trade is only affordable while an append is a *lifecycle* event. What holds it
/// there is [`Appender::observe`]: a caller may hand it a tenant on every commit, and it
/// writes only when the tenant's index set or liveness actually changed.
///
/// The memory of what changed is **per appender instance**, so the fleet-wide rate is
/// lifecycle events × live appenders: a restart re-records each tenant the new appender
/// touches, once. Harmless — the merge is idempotent — but it is the same concentration
/// shape as a bulk import, arriving at the same registers.
#[derive(Debug)]
pub struct Appender<S> {
    store: std::sync::Arc<S>,
    width: Width,
    seen: Mutex<HashMap<TenantId, u64>>,
}

impl<S: BlobStore> Appender<S> {
    /// An appender that has recorded nothing yet.
    pub fn new(store: std::sync::Arc<S>, width: Width) -> Self {
        Self {
            store,
            width,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Records `rec` if it says something new, and returns whether it wrote.
    ///
    /// Costs **one read and one conditional write** uncontended, and **nothing at all** when
    /// the record matches what this appender last wrote for that tenant.
    ///
    /// # Errors
    /// If the store refuses, an object is malformed, or the bucket stays contended.
    pub async fn observe(&self, rec: &TenantRecord) -> Result<bool, CatalogError> {
        let identity = rec.identity();
        if self
            .seen
            .lock()
            .is_ok_and(|s| s.get(&rec.tenant) == Some(&identity))
        {
            return Ok(false);
        }
        self.record(rec).await?;
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(rec.tenant, identity);
        }
        Ok(true)
    }

    /// Records `rec` unconditionally.
    ///
    /// # Errors
    /// If the store refuses, an object is malformed, or the bucket stays contended.
    pub async fn record(&self, rec: &TenantRecord) -> Result<(), CatalogError> {
        crate::require_fencing(self.store.as_ref())?;
        let bucket = bucket_of(rec.tenant, self.width);
        for _ in 0..MAX_CAS_ATTEMPTS {
            let (mut head, tag) = read_head(self.store.as_ref(), bucket).await?;
            // ⚠️ **`merge`, not `retain`.** Two records for one tenant meet in the pointer as
            // well as in the run -- a second appender starts with an empty change map, so a
            // node holding a stale view records unconditionally -- and dropping this tenant's
            // pending entry by id alone lets the older one win. One epoch rule, in one
            // function, applied everywhere records combine.
            let next = merge(head.pending.clone(), std::slice::from_ref(rec));
            // ⚠️ The cap is measured on the RESULT, before anything is written: superseding
            // an entry does not grow the pointer and must not trigger a fold, while a new
            // tenant past the cap folds first so the bound is an invariant of what is
            // *stored* rather than of what a caller remembered to do.
            let outcome = match tag {
                Some(t) if next.len() > MAX_PENDING => {
                    publish(
                        self.store.as_ref(),
                        bucket,
                        &head,
                        t,
                        std::slice::from_ref(rec),
                    )
                    .await?
                }
                tag => {
                    head.pending = next;
                    write_head(self.store.as_ref(), bucket, &head, tag).await?
                }
            };
            if matches!(outcome, Publish::Landed) {
                return Ok(());
            }
        }
        Err(CatalogError::Contended(bucket, MAX_CAS_ATTEMPTS))
    }
}
