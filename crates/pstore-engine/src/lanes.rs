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
    for _ in 0..16 {
        let (current, pre) = match store.get_with_tag(&k).await {
            Ok((bytes, tag)) => (decode(&bytes)?, Precondition::Match(tag)),
            Err(pstore_blob::BlobError::NotFound(_)) => (BTreeSet::new(), Precondition::NotExists),
            Err(e) => return Err(e.into()),
        };
        if current.contains(&lane) {
            return Ok(());
        }
        let mut next = current;
        next.insert(lane);
        match store.put_conditional(&k, encode(&next).into(), pre).await {
            Ok(_) => return Ok(()),
            // A concurrent registration won. Rebase and re-add: overwriting would drop the
            // lane the winner just recorded, and that lane's writes would become
            // unrecoverable without anyone noticing.
            Err(CasError::Lost | CasError::Contended) => {}
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
    let mut base = from;
    let mut window = FIRST_PROBE_WINDOW;
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
