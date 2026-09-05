//! The storage engine: HEAD and the commit protocol, WAL lanes carrying cross-index
//! bundles, and the memtable that makes a write visible before it is folded.

mod bundle;
mod head;
pub mod lanes;

pub use bundle::Entry;
pub use head::{Head, HeadAt, SegmentRef};

use pstore_blob::{BlobStore, Key};
use pstore_format::{Document, Filter, Segment, SegmentWriter};
use pstore_types::{Epoch, LaneId, Seq, TenantId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The key one lane's bundle lives at. **Derived**, so a successor computes it rather than
/// discovering it — which is what keeps recovery off the LIST path.
#[must_use]
pub fn bundle_key(tenant: TenantId, lane: LaneId, seq: Seq) -> Key {
    Key::new(format!(
        "{:04x}/wal/{}/{:016x}/{:016}.bundle",
        tenant.0 as u16, tenant.0, lane.0, seq.0
    ))
}

/// Rows per block in a folded segment. Small enough that zone maps prune usefully, large
/// enough that a scan is not one request per handful of rows.
const ROWS_PER_BLOCK: usize = 64;

/// How many times a commit rebases before giving up.
///
/// Bounded on purpose: a commit that cannot land after this many rebases is reporting
/// contention the caller should know about, not something to keep paying for silently.
const MAX_COMMIT_ATTEMPTS: u32 = 24;

/// Why an engine operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// Another committer won. **Rebase and retry.**
    #[error("commit lost: another writer won")]
    Lost,
    /// The backend could not evaluate the condition. **Retry without rebasing.**
    #[error("commit contended: retry without rebasing")]
    Contended,
    /// HEAD did not decode.
    #[error("HEAD is corrupt")]
    CorruptHead,
    /// A WAL bundle did not decode.
    #[error("WAL bundle is corrupt")]
    CorruptBundle,
    /// A segment could not be read.
    #[error("format error: {0}")]
    Format(String),
    /// The blob store could not serve it.
    #[error("blob error: {0}")]
    Blob(String),
}

impl From<pstore_blob::BlobError> for EngineError {
    fn from(e: pstore_blob::BlobError) -> Self {
        Self::Blob(e.to_string())
    }
}

impl From<pstore_format::FormatError> for EngineError {
    fn from(e: pstore_format::FormatError) -> Self {
        Self::Format(e.to_string())
    }
}

/// Rows written but not yet folded, held in memory and served by queries.
///
/// This is the freshness layer: **visibility does not wait on the fold**, so the flush
/// interval never enters the time-to-searchable budget.
#[derive(Debug, Default)]
struct Memtable {
    /// Buffered but not yet in a bundle.
    pending: BTreeMap<String, Vec<Document>>,
    /// Flushed to a bundle, durable, still not folded into a segment.
    durable: BTreeMap<String, Vec<Document>>,
}

/// One writer's view of one tenant.
#[derive(Debug)]
pub struct Engine<S> {
    store: Arc<S>,
    tenant: TenantId,
    lane: LaneId,
    mem: Mutex<Memtable>,
    /// Next sequence in this lane. Lanes are single-writer, so this needs no coordination
    /// with anyone — which is the entire point of lanes.
    seq: Mutex<Seq>,
    /// Serialises this lane's flushes against each other.
    ///
    /// ⚠️ Not contention control — a lane is single-writer by definition. This *enforces*
    /// that definition against the one caller who ignores it: two overlapping `flush`
    /// calls on the same handle would each reserve a sequence and race to write them, and
    /// a failure of the lower one would leave a gap the tail probe stops at. Cheap to
    /// hold, because the thing it excludes should never happen.
    flushing: tokio::sync::Mutex<()>,
    committed: Mutex<Epoch>,
}

/// The ABA guard: a value that never repeats for two different commits.
///
/// ⚠️ **Load-bearing on any backend whose CAS tag is content-derived** — an S3 ETag on a
/// single-part PUT is the MD5 of the body. Two HEADs that happen to encode identically
/// would then carry identical tags, so a writer that read the first, paused, and woke
/// after the world changed and changed back would have its CAS *accepted*. The nonce makes
/// two commits byte-different even when everything else about them matches.
///
/// XOR, not OR or AND: both of those lose information, so distinct `(epoch, lane)` pairs
/// collapse onto the same nonce and the guard silently stops guarding. Mutation testing
/// found `|` and `&` indistinguishable from `^` to every test in the workspace, which is
/// why `nonces_never_collide_across_epochs_and_lanes` exists.
#[must_use]
pub fn nonce_for(epoch: Epoch, lane: LaneId) -> u64 {
    epoch.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ lane.0
}

/// How long a writer waits before retrying a lost commit.
///
/// ⚠️ Jittered, and that is not decoration. Without it optimistic concurrency
/// **livelocks**: every loser retries immediately, collides with the same peers, and
/// loses again. It surfaced as a *flaky* test rather than a failing one, which is the
/// more expensive way to find out. The jitter comes from the lane rather than a random
/// source so a failing schedule still replays.
///
/// ⚠️ Separated from the sleep on purpose. While this was one function its entire
/// contents were invisible to every test — mutation testing replaced the whole body with
/// nothing, inverted the shift, and swapped the arithmetic, and not one test noticed,
/// because the only observable was elapsed microseconds. A pure function has a contract
/// that can be stated and checked; a sleep does not.
#[must_use]
pub fn backoff_delay(lane: LaneId, attempt: u32) -> std::time::Duration {
    // Doubling, capped: the cap stops a late retry waiting far longer than the operation
    // it is retrying.
    let base = 1u64 << attempt.min(BACKOFF_CAP_SHIFT);
    // 1..=8, never 0: a jitter that can be zero leaves the lanes that draw it colliding
    // in lockstep, which is the livelock this exists to prevent.
    let jitter = (lane.0 % JITTER_SPREAD) + 1;
    std::time::Duration::from_micros(base * jitter)
}

/// Where the doubling stops.
const BACKOFF_CAP_SHIFT: u32 = 6;
/// How many distinct delays a given attempt can produce.
const JITTER_SPREAD: u64 = 8;

async fn backoff(lane: LaneId, attempt: u32) {
    tokio::time::sleep(backoff_delay(lane, attempt)).await;
}

impl<S: BlobStore> Engine<S> {
    /// A writer on one lane of one tenant.
    pub fn new(store: Arc<S>, tenant: TenantId, lane: LaneId) -> Self {
        Self {
            store,
            tenant,
            lane,
            mem: Mutex::new(Memtable::default()),
            seq: Mutex::new(Seq::ZERO),
            flushing: tokio::sync::Mutex::new(()),
            committed: Mutex::new(Epoch::ZERO),
        }
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, Memtable> {
        self.mem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The last epoch this engine committed.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        *self
            .committed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lane_key(&self, seq: Seq) -> Key {
        bundle_key(self.tenant, self.lane, seq)
    }

    fn segment_key(&self, epoch: Epoch, index: &str) -> Key {
        Key::new(format!(
            "{:04x}/tnt/{}/idx/{index}/seg/L0/{:020}-{:016x}.seg",
            self.tenant.0 as u16, self.tenant.0, epoch.0, self.lane.0
        ))
    }

    /// Buffers documents. **Visible immediately**; durable at the next [`Self::flush`].
    pub async fn write(&self, index: &str, docs: Vec<Document>) -> Result<(), EngineError> {
        self.mem()
            .pending
            .entry(index.to_owned())
            .or_default()
            .extend(docs);
        Ok(())
    }

    /// The ids currently buffered and not yet acknowledged.
    ///
    /// Exposed so a recovery test can state precisely which rows a `flush` acknowledges.
    /// Guessing that from the outside would make the test's own bookkeeping the thing
    /// under test.
    #[doc(hidden)]
    pub async fn pending_for_test(&self) -> Vec<String> {
        self.mem()
            .pending
            .values()
            .flatten()
            .map(|d| d.id.clone())
            .collect()
    }

    /// Writes everything buffered as **one bundle object**, whatever it covers.
    ///
    /// `RA = 1 W` for the batch, and for every index in it.
    pub async fn flush(&self) -> Result<Option<Seq>, EngineError> {
        let _lane = self.flushing.lock().await;
        let pending = {
            let mut m = self.mem();
            if m.pending.is_empty() {
                return Ok(None);
            }
            std::mem::take(&mut m.pending)
        };
        let seq = *self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Registered once, on the lane's first flush. A lane nobody can find is a lane
        // whose writes cannot be recovered, and the registration is what makes a successor
        // able to discover it without being told.
        if seq == Seq::ZERO
            && let Err(e) = lanes::register(&*self.store, self.tenant, self.lane).await
        {
            self.restore(pending);
            return Err(e);
        }
        // The one PUT.
        if let Err(e) = self
            .store
            .put(&self.lane_key(seq), bundle::encode(&pending).into())
            .await
        {
            // ⚠️ **The sequence is not consumed, and this is load-bearing** (OQ-91).
            //
            // A lane is recovered by probing forward from the last watermark until a key
            // is missing, so a lane must be DENSE: the first absent sequence is taken as
            // the end. Burning a number on a failed write punches a permanent hole, and
            // every bundle after it — all of them acknowledged, all of them durable —
            // becomes invisible to every future reader. One refused PUT silently
            // truncates the lane forever.
            //
            // Found by the OQ-91 scenario losing two acknowledged rows on seed 0, not by
            // reading this code.
            self.restore(pending);
            return Err(e.into());
        }
        {
            let mut s = self
                .seq
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *s = seq.next();
        }
        // Only now does it move from pending to durable: a write that failed to land must
        // not be reported as durable, and must stay visible so it is not lost.
        let mut m = self.mem();
        for (idx, docs) in pending {
            m.durable.entry(idx).or_default().extend(docs);
        }
        Ok(Some(seq))
    }

    /// Puts a failed flush's rows back in front of anything written since.
    ///
    /// Without this a refused flush would drop the rows it took, which is a silent loss of
    /// data the caller was never told was durable — worse than the error it reports,
    /// because the caller's retry would have no way to know what to retry.
    fn restore(&self, pending: BTreeMap<String, Vec<Document>>) {
        let mut m = self.mem();
        for (idx, mut docs) in pending {
            let slot = m.pending.entry(idx).or_default();
            // Older rows first: they were written first, and a later write to the same id
            // must stay later.
            docs.append(slot);
            *slot = docs;
        }
    }

    /// Replays **every lane's** unfolded WAL bundles into segments and commits them.
    ///
    /// ⚠️ Tenant-scoped, not lane-scoped, and that is the point: folding is work done *on
    /// behalf of the tenant*, so any node may do it and a successor can fold a dead
    /// writer's lane without ever having spoken to it. The lanes come from the registry
    /// and the tails from forward probing — nothing is listed, and nothing is injected.
    ///
    /// ⚠️ **Reads the bundles from the blob store, not from memory.** Folding the in-memory
    /// copy would make the WAL write-only: the objects would be paid for and never read,
    /// and a process that restarted could not recover a single acknowledged write.
    pub async fn fold(&self) -> Result<Epoch, EngineError> {
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let at = head::read(&*self.store, self.tenant).await?;
            let live = lanes::live(&*self.store, self.tenant).await?;

            // Each lane's unfolded span, discovered rather than remembered.
            let spans = futures_util::future::try_join_all(live.iter().map(|l| {
                let from = at.head.watermarks.get(&l.0).copied().unwrap_or(0);
                async move {
                    lanes::tail(&*self.store, self.tenant, *l, from)
                        .await
                        .map(|tail| (*l, from, tail))
                }
            }))
            .await?;

            let keys: Vec<(LaneId, Key)> = spans
                .iter()
                .flat_map(|(lane, from, tail)| {
                    (*from..*tail).map(move |n| (*lane, bundle_key(self.tenant, *lane, Seq(n))))
                })
                .collect();
            if keys.is_empty() {
                return Ok(at.head.epoch);
            }

            let bodies =
                futures_util::future::try_join_all(keys.iter().map(|(_, k)| self.store.get(k)))
                    .await?;

            let mut by_index: BTreeMap<String, Vec<Document>> = BTreeMap::new();
            for body in &bodies {
                for (name, entry) in bundle::read_index(body)? {
                    by_index
                        .entry(name)
                        .or_default()
                        .extend(bundle::read_entry(body, &entry)?);
                }
            }
            if by_index.is_empty() {
                return Ok(at.head.epoch);
            }

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            // The ABA guard: two HEADs differing only in content a content-derived tag is
            // computed from would otherwise share a tag. Derived from the epoch and lane,
            // so it is deterministic and needs no clock.
            next.nonce = nonce_for(next.epoch, self.lane);

            // One segment per index. Folding every index into one object would make each
            // index's ref point at the whole thing, and a scan would return its
            // neighbours' rows.
            for (idx, docs) in &by_index {
                let seg_key = self.segment_key(next.epoch, idx);
                let mut w = SegmentWriter::new(ROWS_PER_BLOCK);
                for d in docs {
                    w.push(d.clone());
                }
                self.store.put(&seg_key, w.finish()).await?;
                next.indexes
                    .entry(idx.clone())
                    .or_default()
                    .push(SegmentRef {
                        key: seg_key.as_str().to_owned(),
                        rows: docs.len() as u32,
                    });
            }
            // ⚠️ Advanced only for the spans actually folded. Advancing a lane past
            // bundles this attempt did not read would drop them permanently — and nothing
            // downstream could tell, because the watermark is the only record of what is
            // outstanding.
            for (lane, _, tail) in &spans {
                if *tail > 0 {
                    next.watermarks.insert(lane.0, *tail);
                }
            }
            // The bundles just folded are now garbage: their rows live in a segment HEAD
            // names. Recorded here rather than deleted here, because a reader holding the
            // previous epoch may still be replaying them.
            next.graveyard
                .entry(next.epoch.0)
                .or_default()
                .extend(keys.iter().map(|(_, k)| k.as_str().to_owned()));

            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    self.mem().durable.clear();
                    *self
                        .committed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = epoch;
                    return Ok(epoch);
                }
                Err(EngineError::Lost | EngineError::Contended)
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    backoff(self.lane, attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Lost)
    }

    /// Reaps objects dereferenced more than `retention` epochs ago.
    ///
    /// **Zero LIST.** GC works from the manifest's graveyard, which records each key at
    /// the epoch it stopped being referenced. Enumerating the bucket would answer a
    /// different question — what *exists*, rather than what is still reachable — and cost
    /// a PUT per thousand keys to answer it wrongly.
    ///
    /// `retention` is a number of epochs, not a duration. A reader that read HEAD at
    /// epoch *e* may take arbitrarily long to finish scanning, so what protects it is not
    /// elapsed time but the guarantee that nothing referenced at *e* is reaped until the
    /// tenant has committed `retention` further epochs.
    ///
    /// Returns how many objects were reaped.
    pub async fn gc(&self, retention: u64) -> Result<usize, EngineError> {
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let at = head::read(&*self.store, self.tenant).await?;
            // Everything dereferenced at an epoch this old is beyond the reach of any
            // reader the window promises to protect.
            let horizon = at.head.epoch.0.saturating_sub(retention);
            let due: Vec<u64> = at
                .head
                .graveyard
                .range(..=horizon)
                .map(|(e, _)| *e)
                .collect();
            if due.is_empty() {
                return Ok(0);
            }
            let live: std::collections::BTreeSet<&str> = at
                .head
                .indexes
                .values()
                .flatten()
                .map(|r| r.key.as_str())
                .collect();
            let doomed: Vec<Key> = due
                .iter()
                .filter_map(|e| at.head.graveyard.get(e))
                .flatten()
                // ⚠️ Checked against what HEAD names *now*, not against what it named when
                // the key was buried. Cheap, and the one thing standing between a bug
                // anywhere in the commit path and deleting live data.
                .filter(|k| !live.contains(k.as_str()))
                .map(|k| Key::new(k.clone()))
                .collect();

            // ⚠️ Deleted BEFORE the manifest is pruned, and the order is not arbitrary.
            // Pruning first and then failing to delete loses the only record that these
            // objects exist, and they leak with nothing left to find them by. Deleting
            // first and then failing to prune costs a repeated delete on the next pass,
            // which is idempotent.
            self.store.delete_batch(&doomed).await?;

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            for e in &due {
                next.graveyard.remove(e);
            }
            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(_) => return Ok(doomed.len()),
                Err(EngineError::Lost | EngineError::Contended)
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    backoff(self.lane, attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Lost)
    }

    /// Commits an arbitrary edit to HEAD.
    ///
    /// Exists for one reason: GC's live-reference guard refuses to reap a key HEAD still
    /// names, and the commit protocol is supposed to make that state unreachable. A
    /// defence that cannot be reached cannot be tested, and an untested defence is one
    /// that quietly stops working — so a test is allowed to construct the state the
    /// protocol forbids, and check that GC survives it.
    #[doc(hidden)]
    pub async fn commit_head_for_test(
        &self,
        mutate: impl FnOnce(&mut Head),
    ) -> Result<Epoch, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        let mut next = at.head.clone();
        next.epoch = next.epoch.next();
        next.nonce = nonce_for(next.epoch, self.lane);
        mutate(&mut next);
        head::commit(&*self.store, self.tenant, &at, &next).await
    }

    /// The published manifest, so a test can assert on what is *referenced* rather than
    /// inferring it from what a scan happens to return.
    #[doc(hidden)]
    pub async fn head_for_test(&self) -> Head {
        head::read(&*self.store, self.tenant)
            .await
            .map(|at| at.head)
            .unwrap_or_default()
    }

    /// The key a compactor writes its merged output to.
    ///
    /// ⚠️ **Carries the compactor's lane**, so two nodes compacting the same inputs write
    /// to two different objects. Deriving the key from the inputs instead would be
    /// tempting — the losers would cost nothing — but it makes the second compactor
    /// overwrite a live object unconditionally, which is Invariant I1 gone. The
    /// create-if-absent that would fix it is exactly the precondition MinIO was *measured*
    /// ignoring, so the fix would be silently absent on a backend we support. A wasted
    /// object that GC reaps is the cheaper mistake.
    fn compacted_key(&self, epoch: Epoch, index: &str) -> Key {
        Key::new(format!(
            "{:04x}/tnt/{}/idx/{index}/seg/L1/{:020}-{:016x}.seg",
            self.tenant.0 as u16, self.tenant.0, epoch.0, self.lane.0
        ))
    }

    /// Merges an index's segments into one, and publishes it by CAS.
    ///
    /// **Optimistic, not coordinated.** Any node may compact any index at any time; there
    /// is no lock, no lease and no claim, because there is nothing to protect. Several
    /// nodes may do the same merge concurrently: they read the same inputs, write their
    /// own outputs, and race to publish. Exactly one CAS lands. The losers discard, and
    /// their objects are unreferenced from the moment they lose, so GC reaps them without
    /// needing to know a compaction ever happened.
    ///
    /// Returns `None` when there was nothing to do — fewer than two segments, or another
    /// compactor got there first.
    ///
    /// `RA = n Rpar + 1 W + 1 commit`, for any *n*.
    pub async fn compact(&self, index: &str) -> Result<Option<Epoch>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        let inputs: Vec<SegmentRef> = at.head.indexes.get(index).cloned().unwrap_or_default();
        if inputs.len() < 2 {
            return Ok(None);
        }
        let keys: Vec<Key> = inputs.iter().map(|r| Key::new(r.key.clone())).collect();

        // Opened and scanned together, like every other multi-segment read: n inputs are
        // n parallel fetches, not n round trips.
        let opened =
            futures_util::future::try_join_all(keys.iter().map(|k| Segment::open(&*self.store, k)))
                .await?;
        let scanned = futures_util::future::try_join_all(
            opened
                .iter()
                .zip(&keys)
                .map(|(seg, k)| seg.scan(&*self.store, k, None)),
        )
        .await?;
        // In input order, so the merged segment reads back in the order the inputs would
        // have. A merge that reorders is a merge that changes the answer.
        let rows: Vec<Document> = scanned.into_iter().flatten().collect();

        let out_key = self.compacted_key(at.head.epoch.next(), index);
        let mut w = SegmentWriter::new(ROWS_PER_BLOCK);
        for d in &rows {
            w.push(d.clone());
        }
        // The single W. Written BEFORE the commit and never rewritten on a retry: a
        // rebase changes which HEAD we condition on, not what we merged.
        self.store.put(&out_key, w.finish()).await?;
        let out = SegmentRef {
            key: out_key.as_str().to_owned(),
            rows: rows.len() as u32,
        };

        let mut at = at;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let current: Vec<SegmentRef> = at.head.indexes.get(index).cloned().unwrap_or_default();
            // ⚠️ The discard condition. If any input is no longer named by HEAD, another
            // compactor published this merge and ours is stale — republishing it would
            // resurrect rows that a later fold may already have superseded. Losing is the
            // normal outcome of optimistic work, so it is not an error.
            if !inputs
                .iter()
                .all(|i| current.iter().any(|c| c.key == i.key))
            {
                return Ok(None);
            }

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            // Segments added since we read: kept, in place, after the merged one. Dropping
            // them would silently discard every row folded while we were merging.
            let mut kept: Vec<SegmentRef> = vec![out.clone()];
            kept.extend(
                current
                    .iter()
                    .filter(|c| !inputs.iter().any(|i| i.key == c.key))
                    .cloned(),
            );
            next.indexes.insert(index.to_owned(), kept);
            // Same rule for the segments this merge replaced.
            next.graveyard
                .entry(next.epoch.0)
                .or_default()
                .extend(inputs.iter().map(|i| i.key.clone()));

            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => return Ok(Some(epoch)),
                Err(e @ (EngineError::Lost | EngineError::Contended))
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    if matches!(e, EngineError::Lost) {
                        at = head::read(&*self.store, self.tenant).await?;
                    }
                    backoff(self.lane, attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Lost)
    }

    /// Every row of an index a filter selects: committed segments plus everything not yet
    /// folded, each appearing **exactly once**.
    pub async fn scan(
        &self,
        index: &str,
        filter: Option<&Filter>,
    ) -> Result<Vec<Document>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        let keys: Vec<Key> = at
            .head
            .indexes
            .get(index)
            .into_iter()
            .flatten()
            .map(|r| Key::new(r.key.clone()))
            .collect();

        // ⚠️ Segments are opened TOGETHER, then scanned together. A loop over segment refs
        // is functionally identical and turns a ten-segment index into a twenty-one-hop
        // query -- measured, not guessed -- which is six times the whole latency budget.
        // Width is free; depth is not.
        let opened =
            futures_util::future::try_join_all(keys.iter().map(|k| Segment::open(&*self.store, k)))
                .await?;
        let scanned = futures_util::future::try_join_all(
            opened
                .iter()
                .zip(&keys)
                .map(|(seg, k)| seg.scan(&*self.store, k, filter)),
        )
        .await?;
        let mut out: Vec<Document> = scanned.into_iter().flatten().collect();
        // Unfolded rows live only in memory. They are not in any segment, so there is
        // nothing to deduplicate against -- the fold clears them in the same step that
        // publishes the segment.
        let m = self.mem();
        for src in [&m.durable, &m.pending] {
            for d in src.get(index).into_iter().flatten() {
                if filter.is_none_or(|f| f.matches(d)) {
                    out.push(d.clone());
                }
            }
        }
        Ok(out)
    }

    /// Exact k-nearest neighbours across everything the index contains.
    pub async fn search(
        &self,
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<(String, f32)>, EngineError> {
        let docs = self.scan(index, filter).await?;
        let mut scored: Vec<(String, f32)> = docs
            .into_iter()
            .map(|d| {
                let dist = d
                    .vector
                    .iter()
                    .zip(query)
                    .map(|(a, b)| (a - b) * (a - b))
                    .sum::<f32>();
                (d.id, dist)
            })
            .collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(k);
        Ok(scored)
    }

    /// Attempts a commit against a **deliberately stale** view of HEAD.
    ///
    /// Exists so the fencing property can be asserted: a caller holding a superseded tag
    /// must be refused. Not part of the engine's real API.
    #[doc(hidden)]
    pub async fn commit_stale_for_test(&self) -> Result<Epoch, EngineError> {
        let stale = HeadAt {
            head: Head::default(),
            tag: None,
        };
        let next = Head {
            epoch: Epoch(1),
            ..Head::default()
        };
        head::commit(&*self.store, self.tenant, &stale, &next).await
    }
}
