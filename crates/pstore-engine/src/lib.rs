//! The storage engine: HEAD and the commit protocol, WAL lanes carrying cross-index
//! bundles, and the memtable that makes a write visible before it is folded.

mod bundle;
mod head;

pub use bundle::Entry;
pub use head::{Head, HeadAt, SegmentRef};

use pstore_blob::{BlobStore, Key};
use pstore_format::{Document, Filter, Segment, SegmentWriter};
use pstore_types::{Epoch, LaneId, Seq, TenantId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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
    committed: Mutex<Epoch>,
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
        Key::new(format!(
            "{:04x}/wal/{}/{:016x}/{:016}.bundle",
            self.tenant.0 as u16, self.tenant.0, self.lane.0, seq.0
        ))
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

    /// Writes everything buffered as **one bundle object**, whatever it covers.
    ///
    /// `RA = 1 W` for the batch, and for every index in it.
    pub async fn flush(&self) -> Result<Option<Seq>, EngineError> {
        let pending = {
            let mut m = self.mem();
            if m.pending.is_empty() {
                return Ok(None);
            }
            std::mem::take(&mut m.pending)
        };
        let seq = {
            let mut s = self
                .seq
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cur = *s;
            *s = s.next();
            cur
        };
        // The one PUT.
        self.store
            .put(&self.lane_key(seq), bundle::encode(&pending).into())
            .await?;
        // Only now does it move from pending to durable: a write that failed to land must
        // not be reported as durable, and must stay visible so it is not lost.
        let mut m = self.mem();
        for (idx, docs) in pending {
            m.durable.entry(idx).or_default().extend(docs);
        }
        Ok(Some(seq))
    }

    /// Replays this lane's unfolded WAL bundles into segments and commits them.
    ///
    /// ⚠️ **Reads the bundles from the blob store, not from memory.** Folding the
    /// in-memory copy would make the WAL write-only: the objects would be paid for and
    /// never read, and a process that restarted could not recover a single acknowledged
    /// write. The memtable exists to make a write *visible*; the bundle is what makes it
    /// *durable*, and only one of those survives a crash.
    ///
    /// Retries a lost CAS by rebasing, which is the protocol: `Lost` means the world
    /// moved, so the attempt is rebuilt against the world that exists now.
    pub async fn fold(&self) -> Result<Epoch, EngineError> {
        let flushed = *self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let at = head::read(&*self.store, self.tenant).await?;
            let from = at.head.watermarks.get(&self.lane.0).copied().unwrap_or(0);
            if from >= flushed.0 {
                // Nothing of ours is unfolded. Another committer may have moved the epoch
                // on, which is not our concern.
                return Ok(at.head.epoch);
            }

            // Every unfolded bundle in this lane, fetched together: the sequence numbers
            // are dense and derived, so there is nothing to discover and nothing to list.
            let keys: Vec<Key> = (from..flushed.0).map(|n| self.lane_key(Seq(n))).collect();
            let bodies =
                futures_util::future::try_join_all(keys.iter().map(|k| self.store.get(k))).await?;

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
            next.nonce = next.epoch.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ self.lane.0;

            // One segment per index. Folding every index into one object would make each
            // index's ref point at the whole thing, and a scan would return its
            // neighbours' rows.
            for (idx, docs) in &by_index {
                let seg_key = self.segment_key(next.epoch, idx);
                let mut w = SegmentWriter::new(ROWS_PER_BLOCK);
                for d in docs {
                    w.push(d.clone());
                }
                // Keyed by the epoch being attempted, so a retry rewrites the same bytes
                // at the same key. A loser leaves an orphan that GC reaps, because no HEAD
                // references it.
                self.store.put(&seg_key, w.finish()).await?;
                next.indexes
                    .entry(idx.clone())
                    .or_default()
                    .push(SegmentRef {
                        key: seg_key.as_str().to_owned(),
                        rows: docs.len() as u32,
                    });
            }
            next.watermarks.insert(self.lane.0, flushed.0);

            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    // Dropped only after the commit lands, so a lost race leaves the rows
                    // visible from the memtable rather than briefly from nowhere.
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
                    // ⚠️ Backoff, and jittered. Without it, optimistic concurrency
                    // LIVELOCKS: every loser retries immediately, collides with the same
                    // peers, and loses again. It showed up here as a flaky test rather
                    // than a failing one, which is the more expensive way to find out.
                    // The jitter is derived from the lane so two writers never wake
                    // together, and needs no clock or randomness to be reproducible.
                    let base = 1u64 << attempt.min(6);
                    let jitter = (self.lane.0 % 8) + 1;
                    tokio::time::sleep(std::time::Duration::from_micros(base * jitter)).await;
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

    /// Sets how far this lane has been flushed, so a successor can replay a predecessor's
    /// bundles.
    ///
    /// ⚠️ A stand-in. In production a successor discovers the tail by forward-probing the
    /// lane and reading the per-shard lane bitmap — neither of which exists until M2 — so
    /// M1 injects the watermark rather than pretending discovery is solved.
    #[doc(hidden)]
    pub fn replay_for_test(&self, flushed: Seq) {
        *self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = flushed;
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
