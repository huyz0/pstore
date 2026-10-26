//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Time travel — M7e.
//!
//! ⚠️ **The whole milestone rests on one invariant**: a segment's key epoch is the epoch it
//! became live at. Given that, an old manifest is a *function of the current one* — the
//! segment keys already carry their birth, and the graveyard already records every burial —
//! so looking backwards costs no archive, no extra PUT, and no extra read.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::{Engine, Head, TimeTravel};
use pstore_format::Document;
use pstore_types::{Epoch, LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

const T: TenantId = TenantId(31);

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5, -0.25, 1.0])
}

/// Every segment key an index names, as a set.
fn keys(head: &Head, index: &str) -> BTreeSet<String> {
    head.indexes
        .get(index)
        .into_iter()
        .flatten()
        .map(|r| r.key.clone())
        .collect()
}

#[tokio::test]
async fn every_epoch_reconstructs_exactly() {
    // ⚠️ The criterion that makes this honest: not "it answered", but **compared against HEADs
    // captured at the time**, at every epoch, over a history containing the three things spec
    // review said a lazy fixture would miss — a compaction, fold-buried WAL bundles whose
    // sequence numbers sit where a loose parser reads an epoch, and unfolded rows still in the
    // memtable when the comparison is made.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    let mut history: Vec<(Epoch, Head)> = Vec::new();

    for i in 0..4 {
        e.write("docs", vec![doc(&format!("d{i}"))]).await.unwrap();
        e.write("other", vec![doc(&format!("o{i}"))]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
        // ⚠️ The epoch comes from the HEAD that was captured, not from the engine: `compact`
        // advances the committed epoch without going through `fold`, so pairing a captured
        // HEAD with a separately-read epoch is how a fixture lies about its own history.
        let h = e.head_for_test().await;
        history.push((h.epoch, h));
    }
    e.compact("docs").await.unwrap();
    let h = e.head_for_test().await;
    history.push((h.epoch, h));

    // ⚠️ Unfolded rows, present at assert time: a reconstruction that fuses the memtable, or a
    // fixture that never has one, is the present wearing a date.
    e.write("docs", vec![doc("unfolded")]).await.unwrap();

    let now = e.head_for_test().await;
    for (epoch, then) in &history {
        let rebuilt = now.as_of(*epoch).expect("within the horizon");
        for index in ["docs", "other"] {
            assert_eq!(
                keys(&rebuilt, index),
                keys(then, index),
                "index {index} at epoch {epoch:?} reconstructed as {:?}, was {:?}",
                keys(&rebuilt, index),
                keys(then, index)
            );
        }
    }
}

#[tokio::test]
async fn a_contended_compaction_still_reconstructs() {
    // ⚠️ **Spec review found this by reading `compact`**: the output key was derived once,
    // before the commit loop, and never re-derived — so a compaction that lost a CAS wrote a
    // key stamped N+1 and committed it at N+2. Reconstructing N+1 then returned the merged
    // segment AND both its inputs: every merged row twice. The fixture forces the retry by
    // letting another writer land between the seal and the commit.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    for i in 0..3 {
        e.write("docs", vec![doc(&format!("d{i}"))]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let before_compaction = e.head_for_test().await;

    // A second writer advances the epoch under the compaction, on a different index so the
    // discard condition does not fire. ⚠️ The interference runs **between the seal and the
    // first commit attempt**: before `compact` it would prove nothing, because the compaction
    // would read the newer HEAD and derive the right key first time.
    let other = Engine::new(Arc::clone(&store), T, LaneId(2));
    other.write("other", vec![doc("x")]).await.unwrap();
    other.flush().await.unwrap();
    let interloper: Arc<std::sync::Mutex<Option<Head>>> = Arc::new(std::sync::Mutex::new(None));
    let seen = Arc::clone(&interloper);
    e.compact_with_interference_for_test("docs", async {
        other.fold().await.unwrap();
        // ⚠️ **The HEAD of the epoch the stale key is stamped with.** That epoch is where the
        // defect shows: the merged segment claims to have been born there while its inputs
        // were still live, so a reconstruction returns both. Asserting only at the epoch
        // before the compaction started passes with the defect present -- measured.
        *seen.lock().unwrap() = Some(other.head_for_test().await);
    })
    .await
    .unwrap();

    let middle = interloper
        .lock()
        .unwrap()
        .clone()
        .expect("the interference ran");
    let now = e.head_for_test().await;
    assert!(
        now.epoch.0 > middle.epoch.0,
        "the fixture did not force a retry"
    );
    for (label, then) in [("before", &before_compaction), ("middle", &middle)] {
        let rebuilt = now.as_of(then.epoch).unwrap();
        assert_eq!(
            keys(&rebuilt, "docs"),
            keys(then, "docs"),
            "at the {label} epoch {:?} the merged segment and its inputs were both returned",
            then.epoch
        );
    }
    // And the merged segment's own key epoch is the epoch it went live at, which is the
    // invariant the whole reconstruction rests on.
    for r in &now.indexes["docs"] {
        let born = key_epoch(&r.key);
        assert!(
            born <= now.epoch.0,
            "segment {} claims to be born at {born}, after the current epoch {:?}",
            r.key,
            now.epoch
        );
        assert!(
            now.as_of(Epoch(born)).unwrap().indexes["docs"]
                .iter()
                .any(|s| s.key == r.key),
            "segment {} is absent from the manifest of the epoch its key names",
            r.key
        );
    }
}

/// The epoch a segment key carries.
fn key_epoch(key: &str) -> u64 {
    key.rsplit('/')
        .next()
        .and_then(|f| f.split('-').next())
        .and_then(|e| e.parse().ok())
        .expect("a segment key carries its epoch")
}

/// Counts reads of the tenant's HEAD object specifically, which is the number acceptance
/// criterion 3 is about.
///
/// ⚠️ **`OpClass::Read` cannot say this**, and the first version of the test below tried: it
/// counts every GET — segment opens, centroid sidecars, blocks — so `> 0` is satisfied by any
/// query at all. Code review inserted two extra `head::read` calls into `query_as_of` and the
/// suite stayed green. A number worth asserting needs a meter that can see it.
#[derive(Debug)]
struct HeadReads<S> {
    inner: S,
    n: Arc<std::sync::atomic::AtomicUsize>,
}

impl<S: pstore_blob::BlobStore> HeadReads<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            n: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
    fn counter(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.n)
    }
    fn count(&self, key: &pstore_blob::Key) {
        if key.as_str().ends_with("/HEAD") {
            self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl<S: pstore_blob::BlobStore> pstore_blob::BlobStore for HeadReads<S> {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &pstore_blob::Key) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.count(key);
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &pstore_blob::Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(
        &self,
        key: &pstore_blob::Key,
        n: u64,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(
        &self,
        key: &pstore_blob::Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        self.count(key);
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(
        &self,
        key: &pstore_blob::Key,
    ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &pstore_blob::Key) -> Result<u64, pstore_blob::BlobError> {
        self.inner.head(key).await
    }
    async fn put(
        &self,
        key: &pstore_blob::Key,
        body: bytes::Bytes,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &pstore_blob::Key,
        body: bytes::Bytes,
        pre: pstore_blob::Precondition,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[pstore_blob::Key]) -> Result<(), pstore_blob::BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(
        &self,
        prefix: &pstore_blob::Key,
    ) -> Result<Vec<pstore_blob::Key>, pstore_blob::BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test]
async fn time_travel_costs_one_head_read_and_no_lists() {
    let meter = HeadReads::new(MemoryStore::new());
    let heads = meter.counter();
    let acct = Arc::new(Accounted::new(meter));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    for i in 0..3 {
        e.write("docs", vec![doc(&format!("d{i}"))]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let at = e.epoch();
    e.write("docs", vec![doc("d9")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let before_lists = acct.count(T, OpClass::List);
    heads.store(0, std::sync::atomic::Ordering::SeqCst);
    let hits = e
        .query_as_of(
            "docs",
            at,
            &[pstore_query::Prefetch::Dense {
                field: pstore_format::DEFAULT_FIELD.to_owned(),
                query: vec![1.0, 0.5, -0.25, 1.0],
                limit: 10,
                tune: pstore_index::vec_index::Query::default(),
            }],
            pstore_query::Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        acct.count(T, OpClass::List) - before_lists,
        0,
        "time travel listed for old keys"
    );
    // ⚠️ **Exactly one.** The reconstruction is arithmetic over the HEAD the query already
    // reads, so a second read means an implementation that asks the store what the past was.
    assert_eq!(
        heads.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a past query read HEAD more than once"
    );
    assert_eq!(hits.hits.len(), 3, "the fourth document did not exist yet");
}

#[tokio::test]
async fn a_query_as_of_sees_what_that_epoch_saw() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("old")]).await.unwrap();
    e.flush().await.unwrap();
    let then = e.fold().await.unwrap();

    e.write("docs", vec![doc("new")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // ⚠️ And an unfolded row, which must not appear in a past answer.
    e.write("docs", vec![doc("unfolded")]).await.unwrap();

    let legs = [pstore_query::Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5, -0.25, 1.0],
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }];
    let past = e
        .query_as_of(
            "docs",
            then,
            &legs,
            pstore_query::Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
        .unwrap();
    let ids: Vec<String> = e.resolve(&past).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["old"], "a past answer carried the present: {ids:?}");
    assert!(
        past.unfolded.is_empty(),
        "the freshness layer was fused into a past answer"
    );
    // ⚠️ And `unfolded_at` is one past the last segment rather than zero: zero is a real
    // segment ordinal, and a hit carrying it resolves against the (empty) unfolded rows and
    // disappears. This test found exactly that.
    assert_eq!(past.unfolded_at, past.segments.len());

    let now = e
        .query("docs", &legs, pstore_query::Fusion::Rrf { k: 60.0 }, 10)
        .await
        .unwrap();
    let mut ids: Vec<String> = e.resolve(&now).into_iter().map(|(id, _)| id).collect();
    ids.sort();
    assert_eq!(ids, ["new", "old", "unfolded"]);
}

#[tokio::test]
async fn an_epoch_below_the_horizon_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    for i in 0..4 {
        e.write("docs", vec![doc(&format!("d{i}"))]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.compact("docs").await.unwrap();
    e.gc(1).await.unwrap();

    let now = e.head_for_test().await;
    let horizon = now.reaped_before;
    assert!(horizon > 0, "the fixture reaped nothing: {now:?}");
    assert!(matches!(
        now.as_of(Epoch(horizon - 1)),
        Err(TimeTravel::Reaped { .. })
    ));
    // ⚠️ The boundary itself is SERVED: a key buried at epoch D is already absent from the
    // manifest at D, so nothing that manifest names has been deleted.
    assert!(
        now.as_of(Epoch(horizon)).is_ok(),
        "the boundary was refused"
    );
}

#[tokio::test]
async fn an_epoch_in_the_future_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let now = e.head_for_test().await;
    // Answering "as of tomorrow" means reporting the present as the past.
    assert!(matches!(
        now.as_of(Epoch(now.epoch.0 + 1)),
        Err(TimeTravel::Future { .. })
    ));
    assert!(now.as_of(now.epoch).is_ok());
}

#[test]
fn the_reap_marker_never_moves_backwards() {
    // ⚠️ Asserted on the function, deliberately: `gc` returns before committing when nothing
    // is due, so the sequence that would move the marker back cannot occur through it — and a
    // test through `gc` would pass without ever running this code.
    let mut h = Head::default();
    h.record_reap(90);
    assert_eq!(h.reaped_before, 90);
    h.record_reap(40);
    assert_eq!(
        h.reaped_before, 90,
        "a horizon that can go backwards is not a bound"
    );
    h.record_reap(120);
    assert_eq!(h.reaped_before, 120);
}

#[tokio::test]
async fn an_index_that_did_not_exist_then_answers_nothing() {
    // ⚠️ Not an error, and not the present: an index created after the epoch asked for simply
    // was not there, so the past manifest names no segment for it and the answer is empty.
    // The API turns that into a `404` with the same predicate a live query uses.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("first", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();
    let then = e.fold().await.unwrap();

    e.write("later", vec![doc("b")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let legs = [pstore_query::Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5, -0.25, 1.0],
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }];
    let answer = e
        .query_as_of(
            "later",
            then,
            &legs,
            pstore_query::Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
        .unwrap();
    assert!(answer.hits.is_empty());
    assert!(
        answer.segments.is_empty(),
        "an index that did not exist named segments: {:?}",
        answer.segments
    );
    // And the one that did exist still answers at that epoch.
    let answer = e
        .query_as_of(
            "first",
            then,
            &legs,
            pstore_query::Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
        .unwrap();
    assert_eq!(answer.hits.len(), 1);
}

#[tokio::test]
async fn a_past_query_refuses_a_wrong_dimension_like_a_present_one() {
    // ⚠️ The error table is the same one: looking backwards does not turn a client error into
    // a server error.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();
    let then = e.fold().await.unwrap();

    let err = e
        .query_as_of(
            "docs",
            then,
            &[pstore_query::Prefetch::Dense {
                field: pstore_format::DEFAULT_FIELD.to_owned(),
                query: vec![1.0, 2.0],
                limit: 10,
                tune: pstore_index::vec_index::Query::default(),
            }],
            pstore_query::Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
        .expect_err("a two-dimensional query was answered over a four-dimensional past");
    assert!(
        matches!(err, pstore_engine::EngineError::DimensionMismatch { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn every_commit_advances_the_epoch_this_engine_reports() {
    // ⚠️ **Measured by code review through the API**: a `gc` that committed epoch 6 answered
    // `"epoch": 5`, and so did every query served from that engine until the next fold —
    // because only `fold` wrote the number down. In a milestone whose whole subject is telling
    // a caller where its history stands, a stale epoch is the wrong number in the one field
    // that matters.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    for i in 0..3 {
        e.write("docs", vec![doc(&format!("d{i}"))]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let committed = |h: &Head| h.epoch;

    e.compact("docs").await.unwrap();
    assert_eq!(
        e.epoch(),
        committed(&e.head_for_test().await),
        "a compaction committed an epoch the engine does not report"
    );

    e.gc(1).await.unwrap();
    assert_eq!(
        e.epoch(),
        committed(&e.head_for_test().await),
        "a reap committed an epoch the engine does not report"
    );
}
