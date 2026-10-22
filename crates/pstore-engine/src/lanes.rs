//! Discovering which lanes exist, and how far each one has been written.
//!
//! A reader that cannot find a lane cannot recover what was written to it, and the whole
//! point of lanes is that writers never coordinate — so discovery has to come from the
//! blob store, without listing and without anyone telling anyone anything.
//!
//! ## ⚠️ Correction to the research
//!
//! `05-storage-engine/write-path-and-wal.md` §3 proposes an **8 KiB bitmap** where a lane
//! claims a slot at `hash(lane_id) % 65536`, so a reader learns the live set in one small
//! GET. That does not work as specified: **a set bit does not name the lane that set it.**
//! Recovering an id from a slot needs either a dictionary — which is the thing the bitmap
//! was meant to replace — or probing a 64-bit id space.
//!
//! Storing the ids directly is both exact and *smaller* at realistic lane counts: 4,096
//! lanes is 32 KiB, against 8 KiB that still cannot answer the question. The bitmap idea
//! assumed a *node*-level lane space where ids are derivable from membership; per tenant,
//! they are not.

use crate::EngineError;
use pstore_blob::{BlobStore, CasError, Key, Precondition};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeSet;

/// Bundles probed in the first round; doubles each round up to [`MAX_PROBE_WINDOW`].
///
/// Probes within a round go out together, so a window is width rather than depth — a
/// larger one costs more cheap reads and never more round trips. It starts small because
/// the common case is a lane with a handful of unfolded bundles, and doubles so a long
/// lane still costs a logarithmic number of rounds rather than a linear one.
const FIRST_PROBE_WINDOW: u64 = 8;

/// Cap on the window, so one round cannot fan out unboundedly.
const MAX_PROBE_WINDOW: u64 = 256;

/// Ceiling on lanes per tenant, so a corrupt object cannot make a reader allocate wildly.
const MAX_LANES: usize = 4096;
/// How many probe rounds before a lane is declared unreadable.
///
/// Generous: with the window doubling to [`MAX_PROBE_WINDOW`] this covers far more
/// bundles than a lane accumulates between folds. It exists to bound the loop, not to
/// limit a lane.
const MAX_PROBE_ROUNDS: u32 = 64;

/// The registry's key, derived from the tenant like every other key here.
pub fn key(tenant: TenantId) -> Key {
    Key::new(format!("{:04x}/tnt/{}/lanes", tenant.0 as u16, tenant.0))
}

fn decode(buf: &[u8]) -> Result<BTreeSet<LaneId>, EngineError> {
    if !buf.len().is_multiple_of(8) || buf.len() / 8 > MAX_LANES {
        return Err(EngineError::CorruptHead);
    }
    Ok(buf
        .chunks_exact(8)
        .filter_map(|c| c.try_into().ok())
        .map(|b| LaneId(u64::from_le_bytes(b)))
        .collect())
}

fn encode(set: &BTreeSet<LaneId>) -> Vec<u8> {
    set.iter().flat_map(|l| l.0.to_le_bytes()).collect()
}

/// Records that `lane` exists, if it is not already recorded.
///
/// **One CAS per lane lifetime, not per write.** A CAS per write would put the commit
/// protocol's contention ceiling back on the write path, which is precisely what lanes
/// exist to avoid.
pub async fn register<S: BlobStore>(
    store: &S,
    tenant: TenantId,
    lane: LaneId,
) -> Result<(), EngineError> {
    crate::require_fencing(store)?;
    let k = key(tenant);
    for attempt in 0..16 {
        let (current, pre) = match store.get_with_tag(&k).await {
            Ok((bytes, tag)) => (decode(&bytes)?, Precondition::Match(tag)),
            Err(pstore_blob::BlobError::NotFound(_)) => (BTreeSet::new(), Precondition::NotExists),
            Err(e) => return Err(e.into()),
        };
        if current.contains(&lane) {
            return Ok(());
        }
        // M123: refused at the ceiling, never written past it -- a registry `decode` refuses
        // breaks the tenant for every reader, where this refuses one writer.
        if current.len() >= MAX_LANES {
            return Err(EngineError::TooManyLanes(current.len()));
        }
        let mut next = current;
        next.insert(lane);
        match store.put_conditional(&k, encode(&next).into(), pre).await {
            Ok(_) => return Ok(()),
            // A concurrent registration won. Rebase and re-add: overwriting would drop the
            // lane the winner just recorded, and that lane's writes would become
            // unrecoverable without anyone noticing.
            //
            // ⚠️ **With backoff, and the budget is unchanged at 16.** Retrying immediately
            // makes every loser re-collide with every other loser, which is how a contended
            // registry turns into a synchronised one: measured under a loaded suite at 100
            // writers registering at once, the tight loop spent all 16 attempts and returned
            // `Lost` to a caller that had done nothing wrong. The bound is what stops an
            // unbounded retry; the delay is what stops the retries from being simultaneous.
            Err(CasError::Lost | CasError::Contended) => {
                crate::backoff(lane, attempt).await;
            }
            Err(CasError::Io(e)) => return Err(EngineError::Blob(e)),
        }
    }
    Err(EngineError::Lost)
}

/// Removes `lane` from the registry, if it is there (M123): what a clean stop does once every
/// bundle on its lane is folded, so later folds stop probing it. The lane's watermark stays in
/// HEAD, so a writer that comes back on it resumes past it -- and registers again.
///
/// # Errors
/// If the store refuses, or every attempt loses.
pub async fn retire<S: BlobStore>(
    store: &S,
    tenant: TenantId,
    lane: LaneId,
) -> Result<(), EngineError> {
    crate::require_fencing(store)?;
    let k = key(tenant);
    for attempt in 0..16 {
        let (mut current, tag) = match store.get_with_tag(&k).await {
            Ok((bytes, tag)) => (decode(&bytes)?, tag),
            Err(pstore_blob::BlobError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if !current.remove(&lane) {
            return Ok(());
        }
        match store
            .put_conditional(&k, encode(&current).into(), Precondition::Match(tag))
            .await
        {
            Ok(_) => return Ok(()),
            // A registration won: rebase, so its lane survives the removal of ours.
            Err(CasError::Lost | CasError::Contended) => crate::backoff(lane, attempt).await,
            Err(CasError::Io(e)) => return Err(EngineError::Blob(e)),
        }
    }
    Err(EngineError::Lost)
}

/// Every lane a writer has ever registered for this tenant. **One read, no LIST.**
pub async fn live<S: BlobStore>(store: &S, tenant: TenantId) -> Result<Vec<LaneId>, EngineError> {
    match store.get(&key(tenant)).await {
        Ok(bytes) => Ok(decode(&bytes)?.into_iter().collect()),
        Err(pstore_blob::BlobError::NotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// How far a lane has been written, discovered by **probing forward** from `from`.
///
/// Returns the first sequence number that does not exist, which is the count of bundles
/// written. Probes in each window go out together, so cost is reads rather than round
/// trips, and a 404 is a normal answer — it is what bounds the lane.
pub async fn tail<S: BlobStore>(
    store: &S,
    tenant: TenantId,
    lane: LaneId,
    from: u64,
) -> Result<u64, EngineError> {
    tail_from(store, tenant, lane, from, FIRST_PROBE_WINDOW).await
}

/// [`tail`], its first round `first` probes wide (M123). A fold probes every lane of the
/// tenant, most of them idle, so it starts at one: an idle lane costs one HEAD where it cost
/// eight, and a lane with bundles one round more. A resume keeps [`FIRST_PROBE_WINDOW`] -- it
/// is on a write's path, where a round costs more than a HEAD.
pub async fn tail_from<S: BlobStore>(
    store: &S,
    tenant: TenantId,
    lane: LaneId,
    from: u64,
    first: u64,
) -> Result<u64, EngineError> {
    let mut base = from;
    let mut window = first.max(1);
    // ⚠️ Bounded, not `loop`. The termination condition is "the store answered 404", which
    // means a store that never does — hostile, buggy, or serving a lane genuinely longer
    // than any tenant could write — spins here forever, on a path a query waits on. An
    // unbounded loop whose exit depends on a remote answer is a liveness bug however
    // correct the logic is; mutation testing surfaced it as a hung test rather than a
    // failing one, which is the expensive way to find out.
    for _ in 0..MAX_PROBE_ROUNDS {
        let keys: Vec<Key> = (base..base + window)
            .map(|n| crate::bundle_key(tenant, lane, pstore_types::Seq(n)))
            .collect();
        // All at once: the window is width, not depth.
        let present = futures_util::future::join_all(keys.iter().map(|k| store.head(k))).await;
        let mut found = 0u64;
        for r in present {
            match r {
                Ok(_) => found += 1,
                // ⚠️ Stops at the FIRST gap rather than counting what exists. A lane is
                // dense by construction, so a hole means the object is missing rather than
                // never written — and skipping past it would silently drop everything
                // before the next present key.
                Err(pstore_blob::BlobError::NotFound(_)) => break,
                Err(e) => return Err(e.into()),
            }
        }
        base += found;
        if found < window {
            return Ok(base);
        }
        window = (window * 2).min(MAX_PROBE_WINDOW);
    }
    Err(EngineError::LaneTooLong(lane))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// ⚠️ The ceiling exists so a corrupt object cannot make a reader allocate wildly, and a
    /// ceiling is only ever wrong at its own edge — so both sides of it are asserted.
    /// Mutation found this: `>` against `>=`, and `/` against `%`, all survived a test that
    /// only tried a small buffer, because every one of them still refuses nothing at that
    /// size. `%` in particular is silent — the multiple-of-8 check above guarantees a
    /// remainder of zero, so the guard would never refuse anything at all.
    #[test]
    fn the_lane_ceiling_refuses_one_past_it_and_accepts_the_last_legal_object() {
        let full: Vec<u8> = (0..MAX_LANES as u64)
            .flat_map(|i| i.to_le_bytes())
            .collect();
        assert_eq!(decode(&full).unwrap().len(), MAX_LANES);

        let mut over = full.clone();
        over.extend_from_slice(&(MAX_LANES as u64).to_le_bytes());
        assert!(decode(&over).is_err(), "one lane past the ceiling decoded");

        // And the other half of the same guard: a length that is not a whole number of ids.
        assert!(decode(&full[..full.len() - 1]).is_err());
    }

    /// M123: the registration past the ceiling wrote a registry `decode` refuses -- and every
    /// fold, strong read and new writer of the tenant failed for good after it. Now the one
    /// lane too many is refused, and the registry stays readable.
    #[tokio::test]
    async fn the_lane_past_the_ceiling_is_refused_and_the_registry_still_reads() {
        let store = pstore_blob::MemoryStore::new();
        let t = TenantId(9);
        let full: BTreeSet<LaneId> = (1..=MAX_LANES as u64).map(LaneId).collect();
        store.put(&key(t), encode(&full).into()).await.unwrap();
        // A lane already in it still registers: idempotent at the ceiling.
        register(&store, t, LaneId(1)).await.unwrap();
        let err = register(&store, t, LaneId(MAX_LANES as u64 + 1))
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::TooManyLanes(_)), "{err:?}");
        assert_eq!(live(&store, t).await.unwrap().len(), MAX_LANES);
    }

    /// M123: a fold probed eight bundles of every lane, idle or not. From its first window of
    /// one, an idle lane costs one HEAD; a lane with bundles still finds its whole tail.
    #[tokio::test]
    async fn a_first_window_of_one_probes_an_idle_lane_once() {
        let store = pstore_testkit::depth::DepthCounting::new(pstore_blob::MemoryStore::new());
        let (t, lane) = (TenantId(9), LaneId(3));
        for n in 0..5 {
            store
                .inner()
                .put(
                    &crate::bundle_key(t, lane, pstore_types::Seq(n)),
                    bytes::Bytes::new(),
                )
                .await
                .unwrap();
        }
        store.reset();
        assert_eq!(tail_from(&store, t, lane, 5, 1).await.unwrap(), 5);
        assert_eq!(store.requests(), 1, "an idle lane");
        store.reset();
        assert_eq!(tail_from(&store, t, lane, 0, 1).await.unwrap(), 5);
        assert_eq!(tail(&store, t, lane, 0).await.unwrap(), 5);
    }
}
