//! One snapshot for every leg of a request (M131): legs run under `Engine::pinned` read one
//! HEAD and one view of the unfolded rows, whatever folds while they run.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Consistency, Engine, EngineError};
use pstore_format::{DEFAULT_FIELD, Document};
use pstore_query::{Aggregate, AggregateSpec, Fusion, OrderBy, Prefetch, Total};
use pstore_types::{CasTag, Epoch, LaneId, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const T: TenantId = TenantId(131);

/// Counts HEAD and lane-registry reads; when armed, the next HEAD read takes its bytes, lets
/// a rival fold and commit on the unwrapped store, and returns the bytes it took. Every HEAD
/// read that comes while that fold runs waits for its commit.
struct Interfering {
    inner: Arc<MemoryStore>,
    rival: Engine<MemoryStore>,
    armed: AtomicBool,
    folding: AtomicBool,
    head_reads: AtomicU64,
    lane_reads: AtomicU64,
}

impl Interfering {
    fn new() -> Arc<Self> {
        let inner = Arc::new(MemoryStore::new());
        Arc::new(Self {
            rival: Engine::new(Arc::clone(&inner), T, LaneId(9)),
            inner,
            armed: AtomicBool::new(false),
            folding: AtomicBool::new(false),
            head_reads: AtomicU64::new(0),
            lane_reads: AtomicU64::new(0),
        })
    }

    fn reads(&self) -> (u64, u64) {
        (
            self.head_reads.load(Ordering::SeqCst),
            self.lane_reads.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl BlobStore for Interfering {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        if key.as_str().ends_with("/lanes") {
            self.lane_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        if !key.as_str().ends_with("/HEAD") {
            return self.inner.get_with_tag(key).await;
        }
        self.head_reads.fetch_add(1, Ordering::SeqCst);
        if self.armed.swap(false, Ordering::SeqCst) {
            self.folding.store(true, Ordering::SeqCst);
            let before = self.inner.get_with_tag(key).await;
            self.rival.fold().await.unwrap();
            self.folding.store(false, Ordering::SeqCst);
            return before;
        }
        while self.folding.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_suffix_as(key, n, class).await
    }
}

fn dense() -> [Prefetch; 1] {
    [Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5],
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }]
}

fn by_id() -> OrderBy {
    OrderBy {
        attr: "id".to_owned(),
        desc: false,
    }
}

fn count() -> AggregateSpec {
    AggregateSpec {
        labels: vec![("n".to_owned(), Aggregate::Count(None))],
        group_by: vec![],
        top_k: 10,
    }
}

/// What three legs of one request saw: each leg's epoch, and its ids or count.
#[derive(Debug)]
struct Legs {
    epochs: [Epoch; 3],
    ranked: Vec<String>,
    ordered: Vec<String>,
    counted: Total,
}

/// A ranked, an ordered and an aggregate leg of `idx`, run together as a request's legs are.
async fn legs<S: BlobStore>(e: &Engine<S>, level: Consistency) -> Result<Legs, EngineError> {
    let prefetch = dense();
    let (a, o, g) = futures_util::future::try_join3(
        e.query_filtered_as("idx", &prefetch, None, Fusion::default(), 10, level),
        e.ordered_as("idx", &by_id(), None, 0, 10, None, level),
        e.aggregate_as("idx", count(), None, None, level),
    )
    .await?;
    Ok(Legs {
        epochs: [a.epoch, o.epoch, g.epoch],
        ranked: a.ids.into_iter().flatten().collect(),
        ordered: o.rows.into_iter().map(|d| d.id).collect(),
        counted: g.groups[0].1[0],
    })
}

/// Folded rows `a` and `b` (epoch 1), and the rival's `c` flushed but not folded.
async fn world() -> (Arc<Interfering>, Engine<Interfering>) {
    let store = Interfering::new();
    let w = Engine::new(Arc::clone(&store.inner), T, LaneId(1));
    w.write(
        "idx",
        vec![
            Document::new("a", vec![1.0, 0.5]),
            Document::new("b", vec![1.0, 0.5]),
        ],
    )
    .await
    .unwrap();
    w.flush().await.unwrap();
    w.fold().await.unwrap();
    store
        .rival
        .write("idx", vec![Document::new("c", vec![1.0, 0.5])])
        .await
        .unwrap();
    store.rival.flush().await.unwrap();
    let r = Engine::new(Arc::clone(&store), T, LaneId(2));
    (store, r)
}

/// Criterion 1: a fold that commits after the snapshot is in no leg.
#[tokio::test]
async fn legs_share_one_snapshot() {
    let (store, r) = world().await;
    store.armed.store(true, Ordering::SeqCst);
    let got = r
        .pinned("idx", Consistency::Eventual, || {
            legs(&r, Consistency::Eventual)
        })
        .await
        .unwrap();
    assert_eq!(got.epochs, [Epoch(1); 3], "{got:?}");
    assert_eq!(got.ranked.len(), 2, "{got:?}");
    assert_eq!(got.ordered, ["a", "b"], "{got:?}");
    assert_eq!(got.counted, Total::Int(2), "{got:?}");
    // The rival did fold: an unpinned read now sees it.
    let after = legs(&r, Consistency::Eventual).await.unwrap();
    assert_eq!(after.epochs, [Epoch(2); 3], "{after:?}");
}

/// Criterion 3: one HEAD read for every leg, and one lane-registry read when strong.
#[tokio::test]
async fn a_pinned_request_reads_head_once() {
    for level in [Consistency::Eventual, Consistency::Strong] {
        let (store, r) = world().await;
        // The rival's unfolded `c` would refuse a strong read: fold it first.
        store.rival.fold().await.unwrap();
        let before = store.reads();
        r.pinned("idx", level, || legs(&r, level)).await.unwrap();
        let (heads, lanes) = store.reads();
        assert_eq!(heads - before.0, 1, "{level:?}");
        let want = u64::from(level == Consistency::Strong);
        assert_eq!(lanes - before.1, want, "{level:?}");
    }
}

/// Criterion 5: a cached HEAD naming a reaped segment is retried once, fresh, for every leg.
#[tokio::test]
async fn a_reaped_bounded_snapshot_retries_fresh() {
    let level = Consistency::Bounded(Duration::from_secs(60));
    let (store, r) = world().await;
    store.rival.fold().await.unwrap();
    // Cached: a HEAD naming the two segments `a, b` and `c` live in.
    legs(&r, level).await.unwrap();
    let w = Engine::new(Arc::clone(&store.inner), T, LaneId(1));
    w.compact("idx").await.unwrap();
    assert!(w.gc(0).await.unwrap() > 0, "nothing reaped");
    let before = store.reads().0;
    let got = r.pinned("idx", level, || legs(&r, level)).await.unwrap();
    assert_eq!(store.reads().0 - before, 1, "{got:?}");
    let e = got.epochs[0];
    assert!(e > Epoch(2), "{got:?}");
    assert_eq!(got.epochs, [e; 3], "{got:?}");
    assert_eq!(got.ordered, ["a", "b", "c"], "{got:?}");
}

/// The SPEC's Delta: under a pin, a read at another level, of another index, or by another
/// engine, reads its own HEAD -- a strong leg still refuses the rival's unfolded row.
#[tokio::test]
async fn other_levels_and_indexes_bypass_the_pin() {
    let (store, r) = world().await;
    let second = Engine::new(Arc::clone(&store), T, LaneId(3));
    let before = store.reads();
    let (strong, other, peer) = r
        .pinned("idx", Consistency::Eventual, || async {
            let strong = r
                .ordered_as("idx", &by_id(), None, 0, 10, None, Consistency::Strong)
                .await;
            let other = r
                .ordered_as("other", &by_id(), None, 0, 10, None, Consistency::Eventual)
                .await;
            let peer = second
                .ordered_as("idx", &by_id(), None, 0, 10, None, Consistency::Eventual)
                .await;
            Ok::<_, EngineError>((strong.err(), other.map(|o| o.epoch), peer.map(|o| o.epoch)))
        })
        .await
        .unwrap();
    assert!(matches!(strong, Some(EngineError::NotFolded)), "{strong:?}");
    assert_eq!(other.unwrap(), Epoch(1));
    assert_eq!(peer.unwrap(), Epoch(1));
    let (heads, lanes) = store.reads();
    // The pin's, the strong leg's own, the other index's own, and the other engine's own.
    assert_eq!(heads - before.0, 4);
    assert_eq!(lanes - before.1, 1);
}
