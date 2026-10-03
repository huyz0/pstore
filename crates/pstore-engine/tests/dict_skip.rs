//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M32: a query asks for no dictionary HEAD says a segment does not have. A HEAD whose refs
//! say nothing -- one from before M32 -- is the baseline: it asks for every dictionary.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Dicts, Engine};
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_query::{Fusion, Prefetch};
use pstore_types::{CasTag, LaneId, TenantId};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const T: TenantId = TenantId(3200);
const THRESHOLD: usize = 64;

/// Counts the reads of each kind of dictionary.
#[derive(Debug, Default)]
struct Counting {
    inner: MemoryStore,
    text: AtomicU64,
    sparse: AtomicU64,
}

impl Counting {
    fn saw(&self, key: &Key) {
        if key.as_str().ends_with(".tdict") {
            self.text.fetch_add(1, Ordering::SeqCst);
        }
        if key.as_str().ends_with(".sdict") {
            self.sparse.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// `(text, sparse)` dictionary reads so far.
    fn counts(&self) -> (u64, u64) {
        (
            self.text.load(Ordering::SeqCst),
            self.sparse.load(Ordering::SeqCst),
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

/// A row with a vector, and -- when `full` -- text and a sparse vector too.
fn doc(i: u32, full: bool) -> Document {
    let x = i as f32;
    let mut d = Document::new(
        format!("d{i:05}"),
        vec![x.sin(), x.cos(), (x * 0.37).sin(), 1.0],
    );
    d.attrs.insert("n".to_owned(), Value::Int(i64::from(i)));
    if full {
        d.attrs.insert(
            "text".to_owned(),
            Value::Str(format!("word{} word{}", i % 5, i % 3)),
        );
        d.vectors.insert(
            "s".to_owned(),
            VectorField::Sparse(vec![(3, Impact::new(0.5)), (i % 7, Impact::new(0.25))]),
        );
    }
    d
}

async fn fold(w: &Engine<Counting>, docs: Vec<Document>) {
    w.write("idx", docs).await.unwrap();
    w.flush().await.unwrap();
    w.fold().await.unwrap();
}

/// Eight vector-only segments, then one with text and a sparse field.
async fn mixed(s: &Arc<Counting>) -> Engine<Counting> {
    let w = engine(s, 1, THRESHOLD);
    for k in 0..8u32 {
        fold(&w, (k * 4..k * 4 + 4).map(|i| doc(i, false)).collect()).await;
    }
    fold(&w, (100..104).map(|i| doc(i, true)).collect()).await;
    w
}

fn text() -> Vec<Prefetch> {
    vec![Prefetch::Text {
        field: "text".to_owned(),
        query: "word2".to_owned(),
        limit: 10,
    }]
}

fn sparse(field: &str) -> Vec<Prefetch> {
    vec![Prefetch::Sparse {
        field: field.to_owned(),
        query: vec![(3, 1.0), (2, 0.5)],
        limit: 10,
    }]
}

fn dense() -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![0.3, 0.9, -0.2, 1.0],
        limit: 10,
        tune: pstore_index::vec_index::Query {
            k: 10,
            exact: true,
            ..pstore_index::vec_index::Query::default()
        },
    }]
}

/// One query's ranked ids and its `(text, sparse)` dictionary reads.
async fn run(s: &Counting, e: &Engine<Counting>, legs: &[Prefetch]) -> (Vec<String>, (u64, u64)) {
    let before = s.counts();
    let a = e.query("idx", legs, Fusion::default(), 10).await.unwrap();
    let after = s.counts();
    let ids = e.resolve(&a).into_iter().map(|(id, _)| id).collect();
    (ids, (after.0 - before.0, after.1 - before.1))
}

/// Forgets every ref's dictionaries, as a HEAD from before M32 had none to remember.
async fn forget(e: &Engine<Counting>) {
    e.commit_head_for_test(|h| {
        for r in h.indexes.values_mut().flatten() {
            r.dicts = None;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn vector_only_segments_are_asked_for_no_dictionary() {
    let s = Arc::new(Counting::default());
    let _w = mixed(&s).await;
    let r = engine(&s, 2, THRESHOLD);
    let (hits, reads) = run(&s, &r, &text()).await;
    assert!(!hits.is_empty());
    assert_eq!(
        reads,
        (1, 0),
        "a text query asked vector-only segments for dictionaries"
    );
    let (hits, reads) = run(&s, &r, &sparse("s")).await;
    assert!(!hits.is_empty());
    assert_eq!(
        reads,
        (0, 1),
        "a sparse query asked vector-only segments for dictionaries"
    );
}

#[tokio::test]
async fn a_sparse_query_over_a_mixed_index_answers() {
    let s = Arc::new(Counting::default());
    let w = mixed(&s).await;
    let a = w
        .query("idx", &sparse("s"), Fusion::default(), 10)
        .await
        .expect("a sparse query over an index with vector-only segments was refused");
    let ids: Vec<String> = w.resolve(&a).into_iter().map(|(id, _)| id).collect();
    assert!(!ids.is_empty());
    assert!(ids.iter().all(|id| id.as_str() >= "d00100"), "{ids:?}");
    // A segment whose sparse field is another is still refused.
    assert!(
        w.query("idx", &sparse("other"), Fusion::default(), 10)
            .await
            .is_err(),
        "a sparse leg for a field the segment does not carry answered"
    );
}

#[tokio::test]
async fn mixed_index_answers_are_unchanged() {
    let s = Arc::new(Counting::default());
    let w = mixed(&s).await;
    let mut fused = dense();
    fused.extend(sparse("s"));
    fused.extend(text());
    let legs = [dense(), sparse("s"), text(), fused];
    let mut known = Vec::new();
    for l in &legs {
        known.push(run(&s, &w, l).await.0);
    }
    forget(&w).await;
    for (l, want) in legs.iter().zip(&known) {
        assert_eq!(&run(&s, &w, l).await.0, want, "{l:?}");
    }
}

#[tokio::test]
async fn a_dictionary_that_exists_is_still_read() {
    let s = Arc::new(Counting::default());
    let w = engine(&s, 1, THRESHOLD);
    for k in 0..3u32 {
        fold(&w, (k * 4..k * 4 + 4).map(|i| doc(i, true)).collect()).await;
    }
    let (_, reads) = run(&s, &w, &text()).await;
    assert_eq!(reads, (3, 0));
    let (_, reads) = run(&s, &w, &sparse("s")).await;
    assert_eq!(reads, (0, 3));
}

#[tokio::test]
async fn an_old_head_and_a_resurrected_ref_still_ask() {
    let s = Arc::new(Counting::default());
    let w = mixed(&s).await;
    let before = w.head_for_test().await.epoch;
    forget(&w).await;
    let (_, reads) = run(&s, &w, &text()).await;
    assert_eq!(reads, (9, 0), "an unknown ref was not asked");
    // Compacted, so the past segments come back from the graveyard, knowing nothing.
    assert!(w.compact("idx").await.unwrap().is_some());
    let from = s.counts();
    w.query_as_of("idx", before, &text(), Fusion::default(), 10)
        .await
        .unwrap();
    assert_eq!(s.counts().0 - from.0, 9, "a resurrected ref was not asked");
}

/// Each segment's recorded dictionaries, against the sidecars in the store.
async fn recorded_match_the_store(s: &Counting, e: &Engine<Counting>, why: &str) {
    let head = e.head_for_test().await;
    let refs: Vec<_> = head.indexes.values().flatten().collect();
    assert!(!refs.is_empty(), "{why}");
    for r in refs {
        let has = |suffix: &str| {
            let k = Key::new(format!("{}{suffix}", r.key));
            async move { s.inner.get(&k).await.is_ok() }
        };
        let want = Dicts {
            sparse: has(".sdict").await,
            text: has(".tdict").await,
        };
        assert_eq!(r.dicts, Some(want), "{why}: {}", r.key);
    }
}

#[tokio::test]
async fn every_maker_records_its_dictionaries() {
    let s = Arc::new(Counting::default());
    let w = mixed(&s).await;
    recorded_match_the_store(&s, &w, "fold").await;
    // A compaction that loses its CAS and re-seals at the next epoch.
    let other = engine(&s, 2, THRESHOLD);
    let got = w
        .compact_with_interference_for_test("idx", async {
            fold(&other, vec![doc(200, false)]).await;
        })
        .await
        .unwrap();
    assert!(got.is_some(), "the retried compaction did not commit");
    recorded_match_the_store(&s, &w, "compaction retry").await;
    assert!(w.compact("idx").await.unwrap().is_some());
    recorded_match_the_store(&s, &w, "compaction").await;
    w.branch("idx", "copy").await.unwrap();
    recorded_match_the_store(&s, &w, "branch").await;
}
