//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M93: a segment's mask, and a segment's open, each on a task of its own.
//!
//! ⚠️ Both ran in the query's own task (M59 left them there): the blocks and footers were
//! fetched concurrently, but each was decoded when its future was polled, on the one thread
//! the query holds. The test runtime's body runs on the test's own thread and its workers on
//! others, so a read that ran on the test's thread ran inline in the query's task.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_format::text::FullText;
use pstore_format::{Document, SegmentWriter};
use pstore_query::{Fusion, Prefetch, Target, query_rows_filtered};
use pstore_types::CasTag;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The read methods a store sees, by the name a query's step uses.
const SUFFIX: &str = "suffix";
const RANGES: &str = "ranges";

/// What a read does after it is recorded.
#[derive(Debug, Clone, Copy)]
enum Then {
    /// Reads at once.
    Read,
    /// Never completes.
    Hang,
}

/// A store recording, per method, the threads its reads ran on and how many are in flight.
#[derive(Debug, Clone)]
struct Seen {
    inner: MemoryStore,
    /// The thread the test body, and so an inline query, runs on.
    driver: std::thread::ThreadId,
    then: Arc<Mutex<(Option<&'static str>, Then)>>,
    threads: Arc<Mutex<BTreeMap<&'static str, BTreeSet<String>>>>,
    on_driver: Arc<Mutex<BTreeMap<&'static str, usize>>>,
    in_flight: Arc<AtomicUsize>,
    /// Hung reads ever begun.
    begun: Arc<AtomicUsize>,
}

/// Counts a read in flight until it is dropped, completed or not.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Seen {
    fn over(inner: MemoryStore) -> Self {
        Self {
            inner,
            driver: std::thread::current().id(),
            then: Arc::new(Mutex::new((None, Then::Read))),
            threads: Arc::default(),
            on_driver: Arc::default(),
            in_flight: Arc::default(),
            begun: Arc::default(),
        }
    }

    /// Hang every read of `method`, from now on.
    fn hang(&self, method: &'static str) {
        *self.then.lock().unwrap() = (Some(method), Then::Hang);
    }

    async fn saw(&self, method: &'static str) {
        let here = std::thread::current().id();
        if here == self.driver {
            *self.on_driver.lock().unwrap().entry(method).or_default() += 1;
        }
        self.threads
            .lock()
            .unwrap()
            .entry(method)
            .or_default()
            .insert(format!("{here:?}"));
        let (which, then) = *self.then.lock().unwrap();
        if which == Some(method) {
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            self.begun.fetch_add(1, Ordering::SeqCst);
            let _guard = InFlight(Arc::clone(&self.in_flight));
            if let Then::Hang = then {
                std::future::pending::<()>().await;
            }
        }
    }

    fn inline(&self, method: &'static str) -> usize {
        self.on_driver
            .lock()
            .unwrap()
            .get(method)
            .copied()
            .unwrap_or(0)
    }

    fn workers(&self, method: &'static str) -> usize {
        self.threads
            .lock()
            .unwrap()
            .get(method)
            .map_or(0, BTreeSet::len)
    }

    fn forget(&self) {
        self.threads.lock().unwrap().clear();
        self.on_driver.lock().unwrap().clear();
    }
}

#[async_trait::async_trait]
impl BlobStore for Seen {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        _: Class,
    ) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.saw(RANGES).await;
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        _: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.saw(SUFFIX).await;
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
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
}

const SEGMENTS: usize = 16;

/// `SEGMENTS` segments of 32 rows each, scanned exactly: no centroid table.
async fn segments(store: &MemoryStore) -> Vec<Target> {
    let mut targets = Vec::new();
    for s in 0..SEGMENTS {
        let mut w = SegmentWriter::new(8);
        for r in 0..32 {
            let x = (s * 32 + r) as f32;
            let mut d = Document::new(
                format!("d{s}-{r}"),
                vec![x.sin(), x.cos(), (x * 0.3).sin(), 1.0],
            );
            d.attrs.insert(
                "n".to_owned(),
                pstore_format::Value::Int((s * 32 + r) as i64),
            );
            w.push(d);
        }
        let key = Key::new(format!("t/idx/{s}.seg"));
        store.put(&key, w.try_finish().unwrap()).await.unwrap();
        targets.push(Target {
            segment_len: None,
            centroids: None,
            deleted: None,
            sparse_dict: false,
            text_dict: false,
            fde_clusters: false,
            shadowed: false,
            segment: key,
        });
    }
    targets
}

fn dense() -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![0.3, 0.9, -0.2, 1.0],
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }]
}

/// A predicate no zone map prunes and no row passes: every segment's mask reads its blocks,
/// and the answer has no rows, so no row round follows to read on the query's thread.
fn nothing() -> pstore_query::Predicate {
    use pstore_query::Predicate;
    Predicate::Absent("n".to_owned())
}

/// A predicate that admits every row.
fn everything() -> pstore_query::Predicate {
    use pstore_query::Predicate;
    Predicate::Absent("never".to_owned())
}

async fn ask<S: BlobStore + Clone>(
    store: &S,
    targets: &[Target],
    filter: &pstore_query::Predicate,
) -> Vec<String> {
    query_rows_filtered(
        store,
        targets,
        &dense(),
        &FullText::default(),
        Some(filter),
        &HashSet::new(),
        Fusion::default(),
        10,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|(h, _, _)| format!("{h:?}"))
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn masks_leave_the_querys_thread() {
    let inner = MemoryStore::new();
    let targets = segments(&inner).await;
    let store = Seen::over(inner.clone());
    // The answer first: a mask moved to a task changes nothing a caller sees.
    let want = ask(&inner, &targets, &everything()).await;
    assert_eq!(ask(&store, &targets, &everything()).await, want);
    assert!(!want.is_empty(), "the control answer is empty");
    store.forget();
    assert!(ask(&store, &targets, &nothing()).await.is_empty());
    assert!(store.workers(RANGES) > 0, "no mask read was seen");
    assert_eq!(
        store.inline(RANGES),
        0,
        "{} mask read(s) ran inline in the query's task",
        store.inline(RANGES)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opens_leave_the_querys_thread() {
    let inner = MemoryStore::new();
    let targets = segments(&inner).await;
    let store = Seen::over(inner);
    assert!(!ask(&store, &targets, &everything()).await.is_empty());
    assert!(store.workers(SUFFIX) > 0, "no footer read was seen");
    assert_eq!(
        store.inline(SUFFIX),
        0,
        "{} footer read(s) ran inline in the query's task",
        store.inline(SUFFIX)
    );
}

/// Drops the query while every read of `method` hangs, and waits for none to be in flight.
async fn dropped_while_hanging(method: &'static str) -> (usize, usize) {
    let inner = MemoryStore::new();
    let targets = segments(&inner).await;
    let store = Seen::over(inner);
    store.hang(method);
    let gave_up = tokio::time::timeout(
        Duration::from_millis(100),
        ask(&store, &targets, &everything()),
    )
    .await;
    assert!(gave_up.is_err(), "a hanging store answered");
    let began = store.begun.load(Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_millis(100);
    while store.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (began, store.in_flight.load(Ordering::SeqCst))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_query_aborts_its_masks() {
    let (began, left) = dropped_while_hanging(RANGES).await;
    assert!(began > 0, "no mask read began");
    assert_eq!(
        left, 0,
        "mask reads still in flight after the query dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_query_aborts_its_opens() {
    let (began, left) = dropped_while_hanging(SUFFIX).await;
    assert!(began > 0, "no footer read began");
    assert_eq!(
        left, 0,
        "footer reads still in flight after the query dropped"
    );
}
