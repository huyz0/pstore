//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M59: every segment's leg on a task of its own.
//!
//! ⚠️ The legs of one query were joined in one task, so each segment's scoring -- run by the
//! index right after its read -- held the only thread the query had, and the runtime's other
//! workers sat idle. A store whose leg reads block their thread stands in for that scoring
//! here: a blocking sleep is a thread held, whoever else wants it, exactly as scoring is.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_format::{Document, SegmentWriter};
use pstore_query::{Fusion, Prefetch, Target, query};
use pstore_types::CasTag;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a leg read does in this store.
#[derive(Debug, Clone, Copy)]
enum Read {
    /// Holds its thread this long, then reads.
    Block(u64),
    /// Never completes.
    Hang,
}

/// A store whose range reads -- the leg round's, never the open round's, which reads a
/// footer by suffix -- block or hang, recording the thread each ran on and how many are in
/// flight.
#[derive(Debug, Clone)]
struct Legs {
    inner: MemoryStore,
    read: Arc<Mutex<Read>>,
    threads: Arc<Mutex<std::collections::BTreeSet<String>>>,
    in_flight: Arc<AtomicUsize>,
    /// The most leg reads ever in flight at once.
    most: Arc<AtomicUsize>,
    reads: Arc<AtomicU64>,
}

/// Counts a read in flight until it is dropped, completed or not.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Legs {
    fn over(inner: MemoryStore) -> Self {
        Self {
            inner,
            read: Arc::new(Mutex::new(Read::Block(0))),
            threads: Arc::default(),
            in_flight: Arc::default(),
            most: Arc::default(),
            reads: Arc::default(),
        }
    }

    async fn leg_read(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        let _guard = InFlight(Arc::clone(&self.in_flight));
        self.threads
            .lock()
            .unwrap()
            .insert(format!("{:?}", std::thread::current().id()));
        let how = *self.read.lock().unwrap();
        match how {
            Read::Block(ms) => std::thread::sleep(Duration::from_millis(ms)),
            Read::Hang => std::future::pending::<()>().await,
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Legs {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.leg_read().await;
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        _: Class,
    ) -> Result<Bytes, BlobError> {
        self.leg_read().await;
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.leg_read().await;
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        _: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.leg_read().await;
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
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
            w.push(Document::new(
                format!("d{s}-{r}"),
                vec![x.sin(), x.cos(), (x * 0.3).sin(), 1.0],
            ));
        }
        let key = Key::new(format!("t/idx/{s}.seg"));
        store.put(&key, w.try_finish().unwrap()).await.unwrap();
        targets.push(Target {
            segment_len: None,
            centroids: None,
            deleted: None,
            sparse_dict: false,
            text_dict: false,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legs_are_scored_in_parallel() {
    let inner = MemoryStore::new();
    let targets = segments(&inner).await;
    let store = Legs::over(inner);
    // Unblocked first: the answer the blocked query must equal, and how many leg reads one
    // query makes -- the serial time below is that many holds, one after another.
    let want = query(&store, &targets, &dense(), Fusion::default(), 10)
        .await
        .unwrap();
    let reads = store.reads.swap(0, Ordering::SeqCst);
    assert!(reads >= SEGMENTS as u64, "{reads} leg reads");
    *store.read.lock().unwrap() = Read::Block(25);
    store.threads.lock().unwrap().clear();
    store.most.store(0, Ordering::SeqCst);
    let started = Instant::now();
    let got = query(&store, &targets, &dense(), Fusion::default(), 10)
        .await
        .unwrap();
    let took = started.elapsed();
    assert_eq!(format!("{got:?}"), format!("{want:?}"));
    // The deterministic part (spec review): two segments' reads held threads at once. In one
    // task, a blocking read holds the only thread the query has, so the most is 1.
    let most = store.most.load(Ordering::SeqCst);
    assert!(most >= 2, "at most {most} leg read(s) ran at once");
    let serial = Duration::from_millis(25 * reads);
    assert!(
        took < serial / 2,
        "{reads} leg reads holding 25 ms each took {took:?}: serial is {serial:?}"
    );
    let threads = store.threads.lock().unwrap().len();
    assert!(threads >= 2, "the leg reads ran on {threads} thread(s)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_query_aborts_its_legs() {
    let inner = MemoryStore::new();
    let targets = segments(&inner).await;
    let store = Legs::over(inner);
    *store.read.lock().unwrap() = Read::Hang;
    let gave_up = tokio::time::timeout(
        Duration::from_millis(100),
        query(&store, &targets, &dense(), Fusion::default(), 10),
    )
    .await;
    assert!(gave_up.is_err(), "a hanging store answered");
    assert!(store.reads.load(Ordering::SeqCst) > 0, "no leg read began");
    // Dropping the query must end its legs: a leg left running reads and scores for nobody.
    let deadline = Instant::now() + Duration::from_millis(100);
    while store.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        store.in_flight.load(Ordering::SeqCst),
        0,
        "leg reads still in flight after their query was dropped"
    );
}

/// A store whose range reads panic: a leg's panic.
#[derive(Debug, Clone)]
struct Panics(MemoryStore);

#[async_trait::async_trait]
impl BlobStore for Panics {
    fn capabilities(&self) -> &Capabilities {
        self.0.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.0.get(key).await
    }
    async fn get_range(&self, _: &Key, _: Range<u64>) -> Result<Bytes, BlobError> {
        panic!("a leg panicked");
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.0.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.0.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.0.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.0.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.0.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.0.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.0.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.0.list_unrestricted(prefix).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[should_panic(expected = "a leg panicked")]
async fn a_legs_panic_is_the_querys() {
    // Code review: a leg's panic on its own task must be resumed in the query's, as an inline
    // one was -- never turned into an error, or lost with the task.
    let inner = MemoryStore::new();
    let targets = segments(&inner).await;
    let _ = query(&Panics(inner), &targets, &dense(), Fusion::default(), 10).await;
}
