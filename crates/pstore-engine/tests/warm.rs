//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `Engine::warm` (M21): an index's metadata, never its bulk, fetched into the read cache in
//! at most three rounds, and only what exists.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_cache::{CacheCore, Caching, DiskConfig};
use pstore_engine::Engine;
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Op, OrderBy, Predicate, Prefetch};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{CasTag, LaneId, TenantId};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const T: TenantId = TenantId(210);
/// The clustering threshold these engines build with: small, so a test can sit on it.
const THRESHOLD: usize = 64;

/// One read that reached the store: its class (`None` for an unclassed ranged read), or
/// `Head` for a plain or tagged GET, and whether it found an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    Head,
    Classed(Option<Class>, bool),
}

/// A store that records every read reaching it.
#[derive(Default)]
struct Recorder {
    inner: MemoryStore,
    reads: Mutex<Vec<Read>>,
}

impl Recorder {
    fn saw<T>(&self, class: Option<Class>, r: &Result<T, BlobError>) {
        self.reads
            .lock()
            .unwrap()
            .push(Read::Classed(class, r.is_ok()));
    }
    fn mark(&self) -> usize {
        self.reads.lock().unwrap().len()
    }
    fn since(&self, mark: usize) -> Vec<Read> {
        self.reads.lock().unwrap()[mark..].to_vec()
    }
}

#[async_trait::async_trait]
impl BlobStore for Recorder {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.reads.lock().unwrap().push(Read::Head);
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        let r = self.inner.get_range(key, range).await;
        self.saw(None, &r);
        r
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        let r = self.inner.get_range(key, range).await;
        self.saw(Some(class), &r);
        r
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        let r = self.inner.get_ranges(key, ranges).await;
        self.saw(None, &r);
        r
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        class: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        let r = self.inner.get_ranges(key, ranges).await;
        self.saw(Some(class), &r);
        r
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        let r = self.inner.get_suffix(key, n).await;
        self.saw(None, &r);
        r
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        let r = self.inner.get_suffix(key, n).await;
        self.saw(Some(class), &r);
        r
    }
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        let r = self.inner.get(key).await;
        self.saw(Some(class), &r);
        r
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
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.reads.lock().unwrap().push(Read::Head);
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

type Cached = Caching<Recorder>;

fn engine(cache: &Arc<Cached>) -> Engine<Cached> {
    Engine::new(Arc::clone(cache), T, LaneId(1)).with_index_params(pstore_index::cluster::Params {
        exact_scan_threshold: THRESHOLD,
        target_list_size: 16,
        ..pstore_index::cluster::Params::default()
    })
}

fn doc(id: String, x: f32, full: bool) -> Document {
    let mut d = Document::new(id, vec![x.sin(), x.cos()]);
    d.attrs.insert("n".to_owned(), Value::Int(x as i64));
    if full {
        d.vectors.insert(
            "s".to_owned(),
            VectorField::Sparse(vec![
                (3, Impact::new(0.5)),
                ((x as u32) % 7, Impact::new(0.25)),
            ]),
        );
        d.attrs.insert(
            "text".to_owned(),
            Value::Str(format!("the quick fox number {}", x as u32 % 5)),
        );
    }
    d
}

async fn fold(e: &Engine<Cached>, docs: Vec<Document>) {
    e.write("idx", docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

/// k segments below the threshold, dense only: no delete vector, no dictionary.
async fn small(store: &Arc<Recorder>, k: usize) -> Arc<Cached> {
    let cache = Arc::new(Caching::new(Arc::clone(store), 64 << 20));
    let e = engine(&cache);
    for s in 0..k {
        fold(
            &e,
            (0..10)
                .map(|i| doc(format!("s{s}-{i}"), (s * 10 + i) as f32, false))
                .collect(),
        )
        .await;
    }
    cache
}

/// Two segments, A of exactly the threshold's rows (clustered) and B below it, both with a
/// sparse and a text field, and one delete vector, on B.
async fn full(store: &Arc<Recorder>) -> Arc<Cached> {
    let cache = Arc::new(Caching::new(Arc::clone(store), 64 << 20));
    let e = engine(&cache);
    fold(
        &e,
        (0..THRESHOLD)
            .map(|i| doc(format!("a{i}"), i as f32, true))
            .collect(),
    )
    .await;
    fold(
        &e,
        (0..10)
            .map(|i| doc(format!("b{i}"), (100 + i) as f32, true))
            .collect(),
    )
    .await;
    e.delete("idx", vec!["b3".into()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    cache
}

/// Every query modality a warm serves: dense, filtered and ordered, and on an index with
/// those fields, sparse and text.
async fn every_query(e: &Engine<Cached>, full: bool) {
    let dense = vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.0],
        limit: 5,
        tune: Query::default(),
    }];
    e.query("idx", &dense, Fusion::default(), 5).await.unwrap();
    let gt = Predicate::Cmp("n".to_owned(), Op::Gt, Value::Int(3));
    e.query_filtered("idx", &dense, Some(&gt), Fusion::default(), 5)
        .await
        .unwrap();
    let by = OrderBy {
        attr: "n".to_owned(),
        desc: false,
    };
    e.ordered("idx", &by, None, 0, 5, None).await.unwrap();
    if !full {
        return;
    }
    let sparse = vec![Prefetch::Sparse {
        field: "s".to_owned(),
        query: vec![(3, 1.0)],
        limit: 5,
    }];
    e.query("idx", &sparse, Fusion::default(), 5).await.unwrap();
    let text = vec![Prefetch::Text {
        field: "text".to_owned(),
        query: "fox".to_owned(),
        limit: 5,
    }];
    e.query("idx", &text, Fusion::default(), 5).await.unwrap();
}

/// The non-bulk reads among `reads`: what a warm is for.
fn metadata(reads: &[Read]) -> Vec<Read> {
    reads
        .iter()
        .copied()
        .filter(|r| matches!(r, Read::Classed(Some(Class::Meta | Class::Pinned), _)))
        .collect()
}

#[tokio::test]
async fn a_warmed_index_opens_without_a_meta_read() {
    for full_world in [false, true] {
        let store = Arc::new(Recorder::default());
        if full_world {
            full(&store).await;
        } else {
            small(&store, 3).await;
        }
        // A fresh cache over the same store, so the writes' own fills do not count.
        let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
        let e = engine(&cache);
        e.warm("idx").await.unwrap();
        let mark = store.mark();
        every_query(&e, full_world).await;
        let after_warm = metadata(&store.since(mark));
        for r in &after_warm {
            assert!(
                matches!(r, Read::Classed(Some(Class::Pinned), false)),
                "full={full_world}: {r:?} reached the store after a warm"
            );
        }

        // An unwarmed twin's second run: exactly the 404s no cache keeps (BACKLOG row 46).
        let twin = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
        let t = engine(&twin);
        every_query(&t, full_world).await;
        let mark = store.mark();
        every_query(&t, full_world).await;
        assert_eq!(
            after_warm.len(),
            metadata(&store.since(mark)).len(),
            "full={full_world}: a warmed query read metadata a self-warmed one did not"
        );
    }
}

#[tokio::test]
async fn a_warm_admits_no_bulk() {
    let store = Arc::new(Recorder::default());
    full(&store).await;
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let e = engine(&cache);
    let mark = store.mark();
    e.warm("idx").await.unwrap();
    for r in store.since(mark) {
        assert!(
            !matches!(r, Read::Classed(None | Some(Class::Bulk | Class::Scan), _)),
            "a warm read {r:?}"
        );
    }
    assert_eq!(cache.resident_in(Class::Bulk), 0, "a warm admitted bulk");
}

#[tokio::test]
async fn a_warm_reads_only_what_exists() {
    let k = 3;
    let store = Arc::new(Recorder::default());
    small(&store, k).await;
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let e = engine(&cache);
    let mark = store.mark();
    let w = e.warm("idx").await.unwrap();
    let reads = store.since(mark);
    assert_eq!(reads.len(), 1 + k, "{reads:?}");
    assert!(
        reads.iter().all(|r| !matches!(r, Read::Classed(_, false))),
        "a warm paid a 404: {reads:?}"
    );
    assert_eq!((w.exists, w.segments, w.fetched), (true, k, 0));
}

#[tokio::test]
async fn every_sidecar_that_exists_is_warmed() {
    let store = Arc::new(Recorder::default());
    full(&store).await;
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let e = engine(&cache);
    let mark = store.mark();
    let w = e.warm("idx").await.unwrap();
    // A: centroids and two dictionaries. B: two dictionaries and its delete vector.
    assert_eq!((w.exists, w.segments, w.fetched), (true, 2, 6));
    let reads = store.since(mark);
    assert!(
        reads.iter().all(|r| !matches!(r, Read::Classed(_, false))),
        "a warm paid a 404: {reads:?}"
    );
    // 1 HEAD, 2 footers, 6 sidecars.
    assert_eq!(reads.len(), 9, "{reads:?}");
}

#[tokio::test]
async fn a_warm_is_three_rounds_deep() {
    let depth = Arc::new(DepthCounting::new(MemoryStore::new()));
    let cache = Arc::new(Caching::new(Arc::clone(&depth), 64 << 20));
    let e = Engine::new(Arc::clone(&cache), T, LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            exact_scan_threshold: THRESHOLD,
            target_list_size: 16,
            ..pstore_index::cluster::Params::default()
        },
    );
    for s in 0..3 {
        let docs = (0..THRESHOLD)
            .map(|i| doc(format!("s{s}-{i}"), (s * 100 + i) as f32, true))
            .collect();
        e.write("idx", docs).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let cold = Arc::new(Caching::new(Arc::clone(&depth), 64 << 20));
    let e = Engine::new(Arc::clone(&cold), T, LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            exact_scan_threshold: THRESHOLD,
            target_list_size: 16,
            ..pstore_index::cluster::Params::default()
        },
    );
    depth.reset();
    let w = e.warm("idx").await.unwrap();
    assert_eq!((w.segments, w.fetched), (3, 9));
    assert!(
        depth.depth() <= 3,
        "a warm was {} rounds deep",
        depth.depth()
    );
}

#[tokio::test]
async fn a_second_warm_costs_head_alone() {
    let store = Arc::new(Recorder::default());
    full(&store).await;
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let e = engine(&cache);
    e.warm("idx").await.unwrap();
    let mark = store.mark();
    e.warm("idx").await.unwrap();
    assert_eq!(store.since(mark), [Read::Head]);
}

#[tokio::test]
async fn an_index_that_does_not_exist_is_not_known() {
    let store = Arc::new(Recorder::default());
    small(&store, 1).await;
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let e = engine(&cache);
    let w = e.warm("nope").await.unwrap();
    assert_eq!((w.exists, w.segments, w.fetched), (false, 0, 0));
    // Unfolded rows in this process: it exists, with nothing to warm.
    e.write("fresh", vec![doc("f".to_owned(), 1.0, false)])
        .await
        .unwrap();
    let w = e.warm("fresh").await.unwrap();
    assert_eq!((w.exists, w.segments, w.fetched), (true, 0, 0));
    // Only a delete, unfolded: as a query decides, that is no index.
    e.delete("gone", vec!["x".into()]).await.unwrap();
    assert!(!e.warm("gone").await.unwrap().exists);
}

/// A fresh directory, removed when dropped.
struct Dir(std::path::PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn on_disk(store: &Arc<Recorder>, dir: &Dir) -> (Arc<CacheCore>, Arc<Cached>) {
    let core = Arc::new(
        CacheCore::open(64 << 20, DiskConfig::new(&dir.0, 1, 64 << 20, "warm-test")).await,
    );
    let cache = Arc::new(Caching::over(Arc::clone(store), Some(Arc::clone(&core))));
    (core, cache)
}

#[tokio::test]
async fn a_warm_survives_a_restart() {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = Dir(std::env::temp_dir().join(format!(
        "pstore-warm-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )));
    let store = Arc::new(Recorder::default());
    full(&store).await;
    let (core, cache) = on_disk(&store, &dir).await;
    engine(&cache).warm("idx").await.unwrap();
    core.close().await;

    let (core, cache) = on_disk(&store, &dir).await;
    let e = engine(&cache);
    let mark = store.mark();
    every_query(&e, true).await;
    for r in metadata(&store.since(mark)) {
        assert!(
            matches!(r, Read::Classed(Some(Class::Pinned), false)),
            "{r:?} reached the store after a restart"
        );
    }
    core.close().await;
}

#[tokio::test]
async fn a_sidecar_the_writer_did_not_write_is_no_error() {
    // Written by an engine that clusters only past 1,000 rows; warmed by one that clusters
    // at THRESHOLD. The warm asks for a centroid table that is not there: a 404, not a failure.
    let store = Arc::new(Recorder::default());
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let writer = Engine::new(Arc::clone(&cache), T, LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            exact_scan_threshold: 1000,
            ..pstore_index::cluster::Params::default()
        },
    );
    let docs = (0..THRESHOLD)
        .map(|i| doc(format!("w{i}"), i as f32, false))
        .collect();
    writer.write("idx", docs).await.unwrap();
    writer.flush().await.unwrap();
    writer.fold().await.unwrap();
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let w = engine(&cache).warm("idx").await.unwrap();
    assert_eq!((w.exists, w.segments, w.fetched), (true, 1, 0));
}

#[tokio::test]
async fn an_index_without_dense_vectors_has_no_centroids_to_ask_for() {
    // THRESHOLD rows, but no dense dimension: the fold clustered nothing.
    let store = Arc::new(Recorder::default());
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let e = engine(&cache);
    let docs = (0..THRESHOLD)
        .map(|i| {
            let mut d = Document::new(format!("z{i}"), vec![]);
            d.vectors.insert(
                "s".to_owned(),
                VectorField::Sparse(vec![(1, Impact::new(0.5))]),
            );
            d
        })
        .collect();
    fold(&e, docs).await;
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let mark = store.mark();
    engine(&cache).warm("idx").await.unwrap();
    let reads = store.since(mark);
    assert!(
        reads.iter().all(|r| !matches!(r, Read::Classed(_, false))),
        "a warm paid a 404: {reads:?}"
    );
}

#[tokio::test]
async fn an_index_another_process_folded_and_dropped_is_not_known() {
    // A's memtable still holds a batch B folded and then dropped: a query on A prunes it
    // against HEAD and finds no index, so a warm must too.
    let store = Arc::new(Recorder::default());
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    let a = Engine::new(Arc::clone(&cache), T, LaneId(1));
    let b = Engine::new(Arc::clone(&cache), T, LaneId(2));
    a.write("x", vec![doc("x1".to_owned(), 1.0, false)])
        .await
        .unwrap();
    a.flush().await.unwrap();
    b.fold().await.unwrap();
    assert!(b.delete_index("x").await.unwrap().is_some());
    assert!(
        !a.warm("x").await.unwrap().exists,
        "a dropped index was warmed"
    );
}

#[tokio::test]
async fn sidecars_are_cached_as_pinned_and_footers_as_meta() {
    let store = Arc::new(Recorder::default());
    full(&store).await;
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    engine(&cache).warm("idx").await.unwrap();
    assert!(cache.resident_in(Class::Pinned) > 0, "no sidecar is pinned");
    assert!(cache.resident_in(Class::Meta) > 0, "no footer is meta");
    // A query's own fills change neither: the warm put each where a query reads it from.
    let (pinned, meta) = (
        cache.resident_in(Class::Pinned),
        cache.resident_in(Class::Meta),
    );
    every_query(&engine(&cache), true).await;
    assert_eq!(
        (
            cache.resident_in(Class::Pinned),
            cache.resident_in(Class::Meta)
        ),
        (pinned, meta)
    );
}

#[tokio::test]
async fn an_unreadable_delete_vector_fails_the_warm() {
    // As it fails the query: answering without it would return rows it deletes.
    let store = Arc::new(Recorder::default());
    full(&store).await;
    let dv: Vec<Key> = store
        .inner
        .list_unrestricted(&Key::new(""))
        .await
        .unwrap()
        .into_iter()
        .filter(|k| k.as_str().ends_with(".dv"))
        .collect();
    assert_eq!(dv.len(), 1, "{dv:?}");
    store.inner.delete_batch(&dv).await.unwrap();
    let cache = Arc::new(Caching::new(Arc::clone(&store), 64 << 20));
    assert!(engine(&cache).warm("idx").await.is_err());
}
