//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M27: a query asks for no centroid table HEAD's row count says is not there. A reader whose
//! threshold is 1 asks for every table, as every query did before M27, and is the baseline.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::Engine;
use pstore_format::{Document, Value};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Op, Predicate, Prefetch};
use pstore_types::{CasTag, LaneId, TenantId};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const T: TenantId = TenantId(2700);
const THRESHOLD: usize = 64;

/// Counts every read, and the reads of a centroid table.
#[derive(Debug, Default)]
struct Counting {
    inner: MemoryStore,
    reads: AtomicU64,
    cen: AtomicU64,
}

impl Counting {
    fn saw(&self, key: &Key) {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if key.as_str().ends_with(".cen") {
            self.cen.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn counts(&self) -> (u64, u64) {
        (
            self.reads.load(Ordering::SeqCst),
            self.cen.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl BlobStore for Counting {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        _: Class,
    ) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.saw(key);
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        _: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.saw(key);
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, _: Class) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_suffix(key, n).await
    }
    async fn get_immutable(&self, key: &Key, _: Class) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get(key).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.saw(key);
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.saw(key);
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.saw(key);
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
}

fn params(threshold: usize) -> pstore_index::cluster::Params {
    pstore_index::cluster::Params {
        exact_scan_threshold: threshold,
        ..pstore_index::cluster::Params::default()
    }
}

fn engine(store: &Arc<Counting>, lane: u64, threshold: usize) -> Engine<Counting> {
    Engine::new(Arc::clone(store), T, LaneId(lane)).with_index_params(params(threshold))
}

fn doc(i: u32) -> Document {
    let x = i as f32;
    let mut d = Document::new(
        format!("d{i:05}"),
        vec![x.sin(), x.cos(), (x * 0.37).sin(), 1.0],
    );
    d.attrs.insert("n".to_owned(), Value::Int(i64::from(i)));
    d.attrs.insert(
        "text".to_owned(),
        Value::Str(format!("common word{} tag{}", i % 5, i % 3)),
    );
    d
}

/// One segment of `n` rows from `start`, folded by a writer at [`THRESHOLD`].
async fn segment(w: &Engine<Counting>, index: &str, start: u32, n: u32) {
    w.write(index, (start..start + n).map(doc).collect())
        .await
        .unwrap();
    w.flush().await.unwrap();
    w.fold().await.unwrap();
}

fn dense(q: [f32; 4], exact: bool) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: q.to_vec(),
        limit: 10,
        tune: Query {
            exact,
            ..Query::default()
        },
    }]
}

/// A dense leg beside a text leg.
fn hybrid(q: [f32; 4], exact: bool) -> Vec<Prefetch> {
    let mut legs = dense(q, exact);
    legs.push(Prefetch::Text {
        field: "text".to_owned(),
        query: "word2 tag1".to_owned(),
        limit: 10,
    });
    legs
}

type Ranked = Vec<(usize, usize, u32)>;

fn ranked(a: &pstore_engine::Answer) -> Ranked {
    a.hits
        .iter()
        .map(|h| (h.segment, h.row, h.score.to_bits()))
        .collect()
}

/// `(reads, centroid reads)` a query made.
async fn cost<F: std::future::Future<Output = R>, R>(s: &Counting, f: F) -> (R, u64, u64) {
    let (r0, c0) = s.counts();
    let out = f.await;
    let (r1, c1) = s.counts();
    (out, r1 - r0, c1 - c0)
}

const Q: [f32; 4] = [0.3, 0.9, -0.2, 1.0];

#[tokio::test]
async fn small_segments_ask_for_no_centroid_table() {
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    for k in 0..8 {
        segment(&w, "idx", k * 4, 4).await;
    }
    let r = engine(&s, 2, THRESHOLD);
    let (a, _, cen) = cost(&s, r.query("idx", &dense(Q, false), Fusion::default(), 10)).await;
    assert_eq!(a.unwrap().hits.len(), 10);
    assert_eq!(cen, 0, "a centroid table no segment has was asked for");
}

#[tokio::test]
async fn a_segment_at_the_threshold_still_uses_its_table() {
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    segment(&w, "idx", 0, THRESHOLD as u32).await;
    let r = engine(&s, 2, THRESHOLD);
    let all = engine(&s, 3, 1);
    // Warm, so each count is the query's own steady cost.
    for e in [&r, &all] {
        e.query("idx", &dense(Q, false), Fusion::default(), 10)
            .await
            .unwrap();
    }
    let (a, mine, cen) = cost(&s, r.query("idx", &dense(Q, false), Fusion::default(), 10)).await;
    assert_eq!(
        cen, 1,
        "the table of a segment at the threshold was not read"
    );
    let (b, before, _) = cost(
        &s,
        all.query("idx", &dense(Q, false), Fusion::default(), 10),
    )
    .await;
    assert_eq!(mine, before, "a segment with a table costs what it did");
    assert_eq!(ranked(&a.unwrap()), ranked(&b.unwrap()));
}

#[tokio::test]
async fn answers_are_unchanged() {
    // Eight small segments and one large, read by a default reader and by one that asks for
    // every table: dense, dense beside a filter, and dense beside a text leg.
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    for k in 0..8 {
        segment(&w, "idx", k * 4, 4).await;
    }
    segment(&w, "idx", 100, THRESHOLD as u32).await;
    let r = engine(&s, 2, THRESHOLD);
    let all = engine(&s, 3, 1);
    let gt = Predicate::Cmp("n".to_owned(), Op::Gt, Value::Int(20));
    for q in [Q, [1.0, 0.0, 0.0, 1.0], [-0.5, 0.5, 0.5, 1.0]] {
        let a = r
            .query("idx", &dense(q, false), Fusion::default(), 10)
            .await
            .unwrap();
        let b = all
            .query("idx", &dense(q, false), Fusion::default(), 10)
            .await
            .unwrap();
        assert_eq!(ranked(&a), ranked(&b));
        let a = r
            .query_filtered("idx", &dense(q, false), Some(&gt), Fusion::default(), 10)
            .await
            .unwrap();
        let b = all
            .query_filtered("idx", &dense(q, false), Some(&gt), Fusion::default(), 10)
            .await
            .unwrap();
        assert_eq!(ranked(&a), ranked(&b));
        let a = r
            .query("idx", &hybrid(q, false), Fusion::default(), 10)
            .await
            .unwrap();
        let b = all
            .query("idx", &hybrid(q, false), Fusion::default(), 10)
            .await
            .unwrap();
        assert_eq!(ranked(&a), ranked(&b));
    }
}

#[tokio::test]
async fn an_as_of_query_still_uses_a_resurrected_table() {
    // A large segment, then a compaction that buries it: `as_of` the epoch it was live
    // resurrects it with an unknown count, and must still read its table.
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    segment(&w, "idx", 0, THRESHOLD as u32).await;
    segment(&w, "idx", 500, 4).await;
    let then = w.head_for_test().await.epoch;
    w.compact("idx").await.unwrap().unwrap();
    let r = engine(&s, 2, THRESHOLD);
    let all = engine(&s, 3, 1);
    let (a, _, cen) = cost(
        &s,
        r.query_as_of("idx", then, &dense(Q, false), Fusion::default(), 10),
    )
    .await;
    assert!(cen >= 1, "the resurrected segment's table was not read");
    let b = all
        .query_as_of("idx", then, &dense(Q, false), Fusion::default(), 10)
        .await
        .unwrap();
    assert_eq!(ranked(&a.unwrap()), ranked(&b));
}

#[tokio::test]
async fn an_as_of_query_over_a_dropped_index_uses_its_tables() {
    // Dropped since: the present HEAD has no schema for it, and its segments come back from
    // the graveyard with unknown counts.
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    segment(&w, "idx", 0, THRESHOLD as u32).await;
    let then = w.head_for_test().await.epoch;
    w.delete_index("idx").await.unwrap().unwrap();
    let r = engine(&s, 2, THRESHOLD);
    let (a, _, cen) = cost(
        &s,
        r.query_as_of("idx", then, &dense(Q, false), Fusion::default(), 10),
    )
    .await;
    assert_eq!(a.unwrap().hits.len(), 10);
    assert!(
        cen >= 1,
        "a dropped index's segment was opened without its table"
    );
}

#[tokio::test]
async fn a_higher_threshold_reader_scans_exactly() {
    // Written at 64 rows, read by a process whose threshold is 1000: it asks for no table,
    // and answers by exact scan -- the true top-k.
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    segment(&w, "idx", 0, THRESHOLD as u32).await;
    let high = engine(&s, 2, 1000);
    let (a, _, cen) = cost(
        &s,
        high.query("idx", &dense(Q, false), Fusion::default(), 10),
    )
    .await;
    assert_eq!(cen, 0);
    let exact = engine(&s, 3, THRESHOLD)
        .query("idx", &dense(Q, true), Fusion::default(), 10)
        .await
        .unwrap();
    assert_eq!(ranked(&a.unwrap()), ranked(&exact));
}

#[tokio::test]
async fn the_saving_is_exactly_the_404s() {
    // Warm: each reader's second query, so every read is the query's own steady cost.
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    for k in 0..8 {
        segment(&w, "idx", k * 4, 4).await;
    }
    let r = engine(&s, 2, THRESHOLD);
    let all = engine(&s, 3, 1);
    for e in [&r, &all] {
        e.query("idx", &dense(Q, false), Fusion::default(), 10)
            .await
            .unwrap();
    }
    let (_, mine, _) = cost(&s, r.query("idx", &dense(Q, false), Fusion::default(), 10)).await;
    let (_, before, cen) = cost(
        &s,
        all.query("idx", &dense(Q, false), Fusion::default(), 10),
    )
    .await;
    assert_eq!(cen, 8);
    assert_eq!(before - mine, 8, "{before} reads before, {mine} now");
}
