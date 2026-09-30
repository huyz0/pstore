//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A bundle is created, never replaced (M17): a second writer on a lane fails loudly rather than
//! erasing the first's durable writes, and a lost acknowledgement is not a second writer.

use bytes::Bytes;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_engine::{Engine, EngineError, bundle_key};
use pstore_format::Document;
use pstore_types::{CasTag, LaneId, Seq, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const T: TenantId = TenantId(170);
const LANE: LaneId = LaneId(1);

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

async fn ids<S: BlobStore + 'static>(e: &Engine<S>) -> Vec<String> {
    let mut ids: Vec<String> = e
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_second_writer_on_a_lane_fails_loudly_and_erases_nothing() {
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), T, LANE);
    let b = Engine::new(Arc::clone(&store), T, LANE);
    a.write("idx", vec![doc("a1")]).await.unwrap();
    assert_eq!(a.flush().await.unwrap(), Some(Seq(0)));
    // B resumes past A's bundle, as M9j made it.
    b.write("idx", vec![doc("b1")]).await.unwrap();
    assert_eq!(b.flush().await.unwrap(), Some(Seq(1)));
    let theirs = store.get(&bundle_key(T, LANE, Seq(1))).await.unwrap();
    // A still believes sequence 1 is its own.
    a.write("idx", vec![doc("a2")]).await.unwrap();
    for _ in 0..2 {
        let err = a.flush().await.expect_err("A overwrote B's bundle");
        assert!(
            matches!(err, EngineError::LaneTaken { lane: 1, seq: 1 }),
            "{err:?}"
        );
        // Loud, and nothing consumed: the rows stay A's to read.
        assert!(ids(&a).await.contains(&"a2".to_owned()));
    }
    assert_eq!(
        store.get(&bundle_key(T, LANE, Seq(1))).await.unwrap(),
        theirs
    );
    // Every row either acknowledged as durable is served after a fold.
    let reader = Engine::new(Arc::clone(&store), T, LaneId(9));
    reader.fold().await.unwrap();
    assert_eq!(ids(&reader).await, ["a1", "b1"]);
}

#[tokio::test]
async fn a_head_read_finds_a_lane_taken_even_after_gc() {
    // B's bundles are folded and reaped, so A's next key is free: only HEAD's watermark says
    // the lane has moved past A, and creating there would put A's rows below it, unread.
    let acct = Accounted::new(MemoryStore::new());
    let a = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    let b = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    a.write("idx", vec![doc("a0")]).await.unwrap();
    assert_eq!(a.flush().await.unwrap(), Some(Seq(0)));
    for id in ["b1", "b2"] {
        b.write("idx", vec![doc(id)]).await.unwrap();
        b.flush().await.unwrap();
    }
    b.fold().await.unwrap();
    b.compact("idx").await.unwrap();
    b.gc(0).await.unwrap();
    let view = acct.as_tenant(T);
    assert!(
        view.get(&bundle_key(T, LANE, Seq(1))).await.is_err(),
        "not reaped"
    );
    // A reads HEAD, as any query does.
    ids(&a).await;
    a.write("idx", vec![doc("a1")]).await.unwrap();
    let writes = acct.count(T, OpClass::Write);
    let err = a.flush().await.expect_err("A wrote below the watermark");
    assert!(
        matches!(err, EngineError::LaneTaken { lane: 1, .. }),
        "{err:?}"
    );
    assert_eq!(acct.count(T, OpClass::Write), writes, "a PUT was issued");
    assert!(view.get(&bundle_key(T, LANE, Seq(1))).await.is_err());
}

/// A correct store whose bundle writes can go wrong once each way: land and then answer `Io`
/// (a lost acknowledgement), answer `Io` without landing, answer `Contended` without landing,
/// or fail the next read of a bundle.
/// Clones share the store and the switches, so a test can keep one while `Accounted` holds
/// another.
#[derive(Debug, Default, Clone)]
struct Unreliable {
    inner: MemoryStore,
    lose_ack: Arc<AtomicBool>,
    fail_before: Arc<AtomicBool>,
    contend: Arc<AtomicBool>,
    fail_read: Arc<AtomicBool>,
    /// Answer the next bundle write `Io` without writing it, and keep it to land later.
    delay: Arc<AtomicBool>,
    delayed: Arc<std::sync::Mutex<Option<(Key, Bytes)>>>,
    /// Land the delayed write when HEAD is next read: between a resolution's GET and its retry.
    land_on_head: Arc<AtomicBool>,
}

fn is_bundle(key: &Key) -> bool {
    key.as_str().ends_with(".bundle")
}

fn io<T>() -> Result<T, BlobError> {
    Err(BlobError::Other("the connection dropped".to_owned()))
}

#[async_trait::async_trait]
impl BlobStore for Unreliable {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        if is_bundle(key) && self.fail_read.swap(false, Ordering::SeqCst) {
            return io();
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
        if key.as_str().ends_with("/HEAD") && self.land_on_head.swap(false, Ordering::SeqCst) {
            let (k, b) = self.delayed.lock().unwrap().take().unwrap();
            self.inner.put(&k, b).await?;
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
        if is_bundle(key) && self.contend.swap(false, Ordering::SeqCst) {
            return Err(CasError::Contended);
        }
        if is_bundle(key) && self.delay.swap(false, Ordering::SeqCst) {
            *self.delayed.lock().unwrap() = Some((key.clone(), body));
            return Err(CasError::Io(
                "timed out; the write is still in flight".to_owned(),
            ));
        }
        if is_bundle(key) && self.fail_before.swap(false, Ordering::SeqCst) {
            return Err(CasError::Io("refused before writing".to_owned()));
        }
        let out = self.inner.put_conditional(key, body, pre).await?;
        if is_bundle(key) && self.lose_ack.swap(false, Ordering::SeqCst) {
            return Err(CasError::Io(
                "the connection dropped after the write".to_owned(),
            ));
        }
        Ok(out)
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
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_immutable(key, class).await
    }
}

async fn ids_of<S: BlobStore + 'static>(e: &Engine<S>, index: &str) -> Vec<String> {
    let mut ids: Vec<String> = e
        .scan(index, None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_lost_acknowledgement_costs_one_get_and_nothing_else() {
    let switches = Unreliable::default();
    let acct = Accounted::new(switches.clone());
    let e = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.write("idx", vec![doc("r1")]).await.unwrap();
    switches.lose_ack.store(true, Ordering::SeqCst);
    let (w, r) = (acct.count(T, OpClass::Write), acct.count(T, OpClass::Read));
    assert_eq!(e.flush().await.unwrap(), Some(Seq(1)));
    assert_eq!(
        (
            acct.count(T, OpClass::Write) - w,
            acct.count(T, OpClass::Read) - r
        ),
        (1, 1)
    );
    e.write("idx", vec![doc("r2")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(2)));
    let fresh = Engine::new(Arc::new(acct.as_tenant(T)), T, LaneId(9));
    fresh.fold().await.unwrap();
    assert_eq!(ids(&fresh).await, ["r0", "r1", "r2"]);
    // A new process on the lane resumes past all three.
    let next = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    next.write("idx", vec![doc("r3")]).await.unwrap();
    assert_eq!(next.flush().await.unwrap(), Some(Seq(3)));
}

#[tokio::test]
async fn an_unresolved_write_is_resolved_first_and_a_drop_takes_its_rows() {
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("x", vec![doc("x0")]).await.unwrap();
    e.write("y", vec![doc("y0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.fold().await.unwrap();
    // Sequence 1 lands with x1 and y1, and neither the write nor the read that would tell
    // succeeds.
    e.write("x", vec![doc("x1")]).await.unwrap();
    e.write("y", vec![doc("y1")]).await.unwrap();
    store.lose_ack.store(true, Ordering::SeqCst);
    store.fail_read.store(true, Ordering::SeqCst);
    e.flush()
        .await
        .expect_err("an unresolved write was reported as landed");
    // `x` is dropped; then more rows, including a new `x`.
    e.delete_index("x").await.unwrap();
    e.write("y", vec![doc("y2")]).await.unwrap();
    e.write("x", vec![doc("xnew")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(2)));
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(9));
    fresh.fold().await.unwrap();
    assert_eq!(ids_of(&fresh, "y").await, ["y0", "y1", "y2"]);
    assert_eq!(ids_of(&fresh, "x").await, ["xnew"]);
}

#[tokio::test]
async fn a_write_that_never_landed_is_written_again_at_its_sequence() {
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    // Folded, so the watermark is exactly the sequence the failed write was at.
    e.fold().await.unwrap();
    e.write("idx", vec![doc("r1")]).await.unwrap();
    store.fail_before.store(true, Ordering::SeqCst);
    store.fail_read.store(true, Ordering::SeqCst);
    e.flush().await.expect_err("the failed write was reported");
    e.write("idx", vec![doc("r2")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(1)));
    assert!(store.inner.get(&bundle_key(T, LANE, Seq(2))).await.is_err());
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(9));
    fresh.fold().await.unwrap();
    assert_eq!(ids(&fresh).await, ["r0", "r1", "r2"]);
}

#[tokio::test]
async fn an_unresolved_write_folded_and_reaped_is_lane_taken_not_a_guess() {
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.write("idx", vec![doc("r1")]).await.unwrap();
    store.lose_ack.store(true, Ordering::SeqCst);
    store.fail_read.store(true, Ordering::SeqCst);
    e.flush()
        .await
        .expect_err("an unresolved write was reported as landed");
    // Another process folds sequence 1 and reaps it before this one resolves it.
    let other = Engine::new(Arc::clone(&store), T, LaneId(9));
    other.fold().await.unwrap();
    other.compact("idx").await.unwrap();
    other.gc(0).await.unwrap();
    assert!(
        store.inner.get(&bundle_key(T, LANE, Seq(1))).await.is_err(),
        "not reaped"
    );
    let err = e
        .flush()
        .await
        .expect_err("an absent bundle past the watermark was guessed");
    assert!(
        matches!(err, EngineError::LaneTaken { lane: 1, seq: 1 }),
        "{err:?}"
    );
}

#[tokio::test]
async fn contention_consumes_no_sequence() {
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.write("idx", vec![doc("r1")]).await.unwrap();
    store.contend.store(true, Ordering::SeqCst);
    let err = e.flush().await.expect_err("contention was not reported");
    assert!(matches!(err, EngineError::Contended), "{err:?}");
    assert_eq!(e.flush().await.unwrap(), Some(Seq(1)));
}

#[tokio::test]
async fn a_flush_that_creates_its_bundle_reads_nothing() {
    let acct = Accounted::new(MemoryStore::new());
    let a = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    let b = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    a.write("idx", vec![doc("a0")]).await.unwrap();
    a.flush().await.unwrap();
    let count = |class| acct.count(T, class);
    let (w, r) = (count(OpClass::Write), count(OpClass::Read));
    a.write("idx", vec![doc("a1")]).await.unwrap();
    assert_eq!(a.flush().await.unwrap(), Some(Seq(1)));
    assert_eq!(
        (count(OpClass::Write) - w, count(OpClass::Read) - r),
        (1, 0)
    );
    // B takes sequence 2; A's attempt there costs its PUT and nothing else.
    b.write("idx", vec![doc("b0")]).await.unwrap();
    assert_eq!(b.flush().await.unwrap(), Some(Seq(2)));
    let (w, r) = (count(OpClass::Write), count(OpClass::Read));
    a.write("idx", vec![doc("a2")]).await.unwrap();
    assert!(a.flush().await.is_err());
    assert_eq!(
        (count(OpClass::Write) - w, count(OpClass::Read) - r),
        (1, 0)
    );
}

#[tokio::test]
async fn a_fold_inside_a_flush_is_no_second_writer() {
    // A fold commits a watermark past the bundle this flush just wrote, and this engine reads
    // that HEAD, all before the flush has advanced `next`. Nothing else writes the lane.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    let other = Engine::new(Arc::clone(&store), T, LaneId(9));
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.write("idx", vec![doc("r1")]).await.unwrap();
    let flushed = e
        .flush_with_interference_for_test(async {
            other.fold().await.unwrap();
            ids(&e).await;
        })
        .await
        .unwrap();
    assert_eq!(flushed, Some(Seq(1)));
    ids(&e).await;
    e.write("idx", vec![doc("r2")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(2)));
    // A process that reads HEAD before it has ever flushed resumes past the watermark.
    let late = Engine::new(Arc::clone(&store), T, LANE);
    other.fold().await.unwrap();
    ids(&late).await;
    late.write("idx", vec![doc("r3")]).await.unwrap();
    assert_eq!(late.flush().await.unwrap(), Some(Seq(3)));
}

#[tokio::test]
async fn a_write_that_lands_late_is_its_own_not_a_second_writer() {
    // The PUT times out and is still in flight: the resolving read finds nothing, so the flush
    // fails. Then it lands. The next flush's first attempt there meets `Lost`, and the bytes
    // say it was this process's own.
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.write("idx", vec![doc("r1")]).await.unwrap();
    store.delay.store(true, Ordering::SeqCst);
    e.flush()
        .await
        .expect_err("a timed-out write was reported as landed");
    let (key, body) = store.delayed.lock().unwrap().take().unwrap();
    store.inner.put(&key, body).await.unwrap();
    e.write("idx", vec![doc("r2")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(1)));
    assert_eq!(e.flush().await.unwrap(), Some(Seq(2)));
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(9));
    fresh.fold().await.unwrap();
    assert_eq!(ids(&fresh).await, ["r0", "r1", "r2"]);
}

#[tokio::test]
async fn another_writers_bundle_where_a_write_is_unresolved_is_lane_taken() {
    // A's write at 1 fails before landing, and so does the read that would tell. B, on the
    // same lane, then writes 1. A's resolution finds a bundle there that is not its bytes.
    let store = Arc::new(Unreliable::default());
    let a = Engine::new(Arc::clone(&store), T, LANE);
    a.write("idx", vec![doc("a0")]).await.unwrap();
    assert_eq!(a.flush().await.unwrap(), Some(Seq(0)));
    a.write("idx", vec![doc("a1")]).await.unwrap();
    store.fail_before.store(true, Ordering::SeqCst);
    store.fail_read.store(true, Ordering::SeqCst);
    a.flush().await.expect_err("the failed write was reported");
    let b = Engine::new(Arc::clone(&store), T, LANE);
    b.write("idx", vec![doc("b1")]).await.unwrap();
    assert_eq!(b.flush().await.unwrap(), Some(Seq(1)));
    let err = a.flush().await.expect_err("A took B's bundle for its own");
    assert!(
        matches!(err, EngineError::LaneTaken { lane: 1, seq: 1 }),
        "{err:?}"
    );
    // A's row was never durable, and stays A's to read.
    assert!(ids(&a).await.contains(&"a1".to_owned()));
}

#[tokio::test]
async fn another_writer_where_a_write_was_found_absent_is_lane_taken() {
    // A's write at 1 fails before landing, and its read finds nothing there, so A will write 1
    // again. B takes 1 first. A's attempt meets `Lost`, and B's bytes are not A's.
    let store = Arc::new(Unreliable::default());
    let a = Engine::new(Arc::clone(&store), T, LANE);
    a.write("idx", vec![doc("a0")]).await.unwrap();
    assert_eq!(a.flush().await.unwrap(), Some(Seq(0)));
    a.write("idx", vec![doc("a1")]).await.unwrap();
    store.fail_before.store(true, Ordering::SeqCst);
    a.flush().await.expect_err("the failed write was reported");
    let b = Engine::new(Arc::clone(&store), T, LANE);
    b.write("idx", vec![doc("b1")]).await.unwrap();
    assert_eq!(b.flush().await.unwrap(), Some(Seq(1)));
    let err = a.flush().await.expect_err("A took B's bundle for its own");
    assert!(
        matches!(err, EngineError::LaneTaken { lane: 1, seq: 1 }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_lane_found_taken_stays_taken_after_the_collision_is_reaped() {
    // A learns its lane is taken, then only writes. B's bundles are folded and reaped, so A's
    // key is free again -- and creating there would put A's rows below the watermark, unread.
    let acct = Accounted::new(MemoryStore::new());
    let a = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    let b = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    a.write("idx", vec![doc("a0")]).await.unwrap();
    assert_eq!(a.flush().await.unwrap(), Some(Seq(0)));
    b.write("idx", vec![doc("b1")]).await.unwrap();
    assert_eq!(b.flush().await.unwrap(), Some(Seq(1)));
    a.write("idx", vec![doc("a1")]).await.unwrap();
    assert!(matches!(
        a.flush().await,
        Err(EngineError::LaneTaken { seq: 1, .. })
    ));
    for id in ["b2", "b3"] {
        b.write("idx", vec![doc(id)]).await.unwrap();
        b.flush().await.unwrap();
    }
    b.fold().await.unwrap();
    b.compact("idx").await.unwrap();
    b.gc(0).await.unwrap();
    let view = acct.as_tenant(T);
    assert!(
        view.get(&bundle_key(T, LANE, Seq(1))).await.is_err(),
        "not reaped"
    );
    let writes = acct.count(T, OpClass::Write);
    let err = a.flush().await.expect_err("A created below the watermark");
    assert!(matches!(err, EngineError::LaneTaken { .. }), "{err:?}");
    assert_eq!(acct.count(T, OpClass::Write), writes, "a PUT was issued");
}

#[tokio::test]
async fn the_first_of_two_timed_out_writes_landing_late_is_its_own() {
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    // The first attempt at 1 times out in flight; the second fails outright.
    e.write("idx", vec![doc("r1")]).await.unwrap();
    store.delay.store(true, Ordering::SeqCst);
    e.flush()
        .await
        .expect_err("a timed-out write was reported as landed");
    e.write("idx", vec![doc("r2")]).await.unwrap();
    store.fail_before.store(true, Ordering::SeqCst);
    e.flush()
        .await
        .expect_err("a failed write was reported as landed");
    // Then the first lands.
    let (key, body) = store.delayed.lock().unwrap().take().unwrap();
    store.inner.put(&key, body).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(1)));
    assert_eq!(e.flush().await.unwrap(), Some(Seq(2)));
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(9));
    fresh.fold().await.unwrap();
    assert_eq!(ids(&fresh).await, ["r0", "r1", "r2"]);
}

#[tokio::test]
async fn a_late_write_folded_before_its_retry_is_its_own() {
    // The write found absent lands late and is folded; this engine reads that HEAD. One GET
    // says the bundle is its own, so the watermark past it is no second writer.
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.write("idx", vec![doc("r1")]).await.unwrap();
    store.delay.store(true, Ordering::SeqCst);
    e.flush()
        .await
        .expect_err("a timed-out write was reported as landed");
    let (key, body) = store.delayed.lock().unwrap().take().unwrap();
    store.inner.put(&key, body).await.unwrap();
    let other = Engine::new(Arc::clone(&store), T, LaneId(9));
    other.fold().await.unwrap();
    ids(&e).await;
    e.write("idx", vec![doc("r2")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(2)));
    other.fold().await.unwrap();
    assert_eq!(ids(&other).await, ["r0", "r1", "r2"]);
}

#[tokio::test]
async fn a_write_landing_during_its_own_resolution_is_its_own() {
    // The resolution reads the bundle as absent; the late PUT lands while it reads HEAD; the
    // retry then meets `Lost`, and the record as it now stands -- absent -- says whose.
    let store = Arc::new(Unreliable::default());
    let e = Engine::new(Arc::clone(&store), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.write("idx", vec![doc("r1")]).await.unwrap();
    store.delay.store(true, Ordering::SeqCst);
    store.fail_read.store(true, Ordering::SeqCst);
    e.flush()
        .await
        .expect_err("an unresolved write was reported as landed");
    store.land_on_head.store(true, Ordering::SeqCst);
    assert_eq!(e.flush().await.unwrap(), Some(Seq(1)));
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(9));
    fresh.fold().await.unwrap();
    assert_eq!(ids(&fresh).await, ["r0", "r1"]);
}

#[tokio::test]
async fn an_absent_write_at_the_watermark_costs_one_put() {
    // Nothing in HEAD says the lane moved past it, so no read is spent resolving it again.
    let switches = Unreliable::default();
    let acct = Accounted::new(switches.clone());
    let e = Engine::new(Arc::new(acct.as_tenant(T)), T, LANE);
    e.write("idx", vec![doc("r0")]).await.unwrap();
    assert_eq!(e.flush().await.unwrap(), Some(Seq(0)));
    e.fold().await.unwrap();
    e.write("idx", vec![doc("r1")]).await.unwrap();
    switches.fail_before.store(true, Ordering::SeqCst);
    e.flush().await.expect_err("the failed write was reported");
    let (w, r) = (acct.count(T, OpClass::Write), acct.count(T, OpClass::Read));
    assert_eq!(e.flush().await.unwrap(), Some(Seq(1)));
    assert_eq!(
        (
            acct.count(T, OpClass::Write) - w,
            acct.count(T, OpClass::Read) - r
        ),
        (1, 0)
    );
}
