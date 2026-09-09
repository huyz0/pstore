//! Listing every tenant, in two rounds and no LISTs.

use crate::bucket::{read_head, read_run};
use crate::keys::Width;
use crate::record::{State, TenantRecord};
use crate::{CatalogError, bucket::merge};
use pstore_blob::BlobStore;
use pstore_types::Epoch;
use std::collections::BTreeMap;

/// What a reader remembers about one bucket, so the next pass can skip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// The run epoch that was current.
    pub run_epoch: Epoch,
    /// Its content digest. Compared as well as the epoch, so a rewritten run at the same
    /// epoch — which the run key makes possible — is not mistaken for the same one.
    pub digest: u64,
}

/// The answer, plus what to hand back for an incremental pass.
#[derive(Debug, Clone, Default)]
pub struct Enumeration {
    /// Live tenants, ordered by id. Tombstoned ones are filtered here rather than dropped
    /// from the run — see [`State::Deleted`].
    ///
    /// ⚠️ From [`enumerate`] this is the whole catalog; from [`enumerate_since`] it is a
    /// **delta** — a bucket whose run has not moved contributes only its pending records, so
    /// a caller that treats this as a census sees a short answer that looks complete.
    pub records: Vec<TenantRecord>,
    /// Buckets whose run was read this pass.
    pub read: Vec<u32>,
    /// Every bucket that exists, with the run it named.
    pub marks: BTreeMap<u32, Mark>,
}

/// Every tenant in the deployment.
///
/// **Two rounds**: every bucket pointer at once, then every run they name at once. Request
/// count is `width + runs` — a function of the shape, never of the tenant count.
///
/// # Errors
/// If the store refuses a read, an object is malformed, or a head names a run that is gone.
/// ⚠️ A refusal is an **error**, never a shorter answer: derived head keys 404 by design, so
/// absence is the one failure that may be swallowed and nothing else is.
pub async fn enumerate<S: BlobStore>(store: &S, width: Width) -> Result<Enumeration, CatalogError> {
    enumerate_since(store, width, &BTreeMap::new()).await
}

/// Every tenant, skipping the runs a previous pass already read.
///
/// The pointers are read either way — that is the round that makes the cost independent of
/// tenant count — but a bucket whose run has not moved contributes only its pending records.
///
/// # Errors
/// As [`enumerate`].
pub async fn enumerate_since<S: BlobStore>(
    store: &S,
    width: Width,
    prior: &BTreeMap<u32, Mark>,
) -> Result<Enumeration, CatalogError> {
    // Round one: every pointer, issued together. Width is fan-out, not depth.
    let heads = futures_util::future::try_join_all(
        width
            .all()
            .map(|b| async move { read_head(store, b).await }),
    )
    .await?;

    let mut out = Enumeration::default();
    let mut wanted = Vec::new();
    let mut pending_only = Vec::new();
    for (bucket, (head, _)) in width.all().zip(heads) {
        let mark = Mark {
            run_epoch: head.run_epoch,
            digest: head.digest,
        };
        if head.run_epoch == Epoch::ZERO && head.pending.is_empty() {
            continue;
        }
        out.marks.insert(bucket, mark);
        if head.run_epoch != Epoch::ZERO && prior.get(&bucket) != Some(&mark) {
            wanted.push((bucket, head));
        } else {
            pending_only.push(head);
        }
    }

    // Round two: the runs, also issued together.
    let runs = futures_util::future::try_join_all(
        wanted
            .iter()
            .map(|(b, head)| async move { read_run(store, *b, head).await }),
    )
    .await?;

    let mut all = Vec::new();
    for ((bucket, head), run) in wanted.iter().zip(runs) {
        out.read.push(*bucket);
        all.extend(merge(run, head.pending.iter()));
    }
    for head in &pending_only {
        all.extend(head.pending.iter().cloned());
    }
    all.sort_by_key(|r| r.tenant);
    all.retain(|r| r.state == State::Live);
    out.records = all;
    Ok(out)
}
