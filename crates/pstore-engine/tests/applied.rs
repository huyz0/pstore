//! Applied write completion (M129): a request that waits for the commit that folds it, and
//! reports what that fold decided -- summed over every commit, found by any engine.

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
use pstore_engine::{
    Applied, Engine, Metric, OUTCOMES_KEPT, OperationId, Outcome, Patch, applying,
};
use pstore_format::{Document, Value};
use pstore_query::{Op, Predicate};
use pstore_types::{CasTag, LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const T: TenantId = TenantId(129);

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

fn known(a: Applied) -> Outcome {
    match a {
        Applied::Known(o) => o,
        Applied::Unavailable => panic!("an outcome reported unavailable"),
    }
}

/// Counts what the tests assert on, and lets a rival fold before one HEAD commit.
struct Watched {
    inner: Arc<MemoryStore>,
    rival: Engine<MemoryStore>,
    /// The next HEAD commit is preceded by a rival fold of everything, so it loses.
    overtake: AtomicBool,
    head_commits: AtomicU64,
    head_reads: AtomicU64,
    outcome_puts: AtomicU64,
    outcome_reads: AtomicU64,
}

impl Watched {
    fn new() -> Arc<Self> {
        let inner = Arc::new(MemoryStore::new());
        Arc::new(Self {
            rival: Engine::new(Arc::clone(&inner), T, LaneId(9)),
            inner,
            overtake: AtomicBool::new(false),
            head_commits: AtomicU64::new(0),
            head_reads: AtomicU64::new(0),
            outcome_reads: AtomicU64::new(0),
            outcome_puts: AtomicU64::new(0),
        })
    }
}

#[async_trait::async_trait]
impl BlobStore for Watched {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        if key.as_str().contains("/out/") {
            self.outcome_reads.fetch_add(1, Ordering::SeqCst);
            // Slow enough that concurrent waiters' reads overlap, if they are not shared.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
        if key.as_str().ends_with("/HEAD") {
            self.head_reads.fetch_add(1, Ordering::SeqCst);
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
        if key.as_str().ends_with("/HEAD") {
            self.head_commits.fetch_add(1, Ordering::SeqCst);
            if self.overtake.swap(false, Ordering::SeqCst) {
                // Through the inner store, so it never comes back here.
                self.rival.fold().await.unwrap();
            }
        }
        if key.as_str().contains("/out/") {
            self.outcome_puts.fetch_add(1, Ordering::SeqCst);
        }
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

/// Seeds `idx` with rows `a` (n = 1) and `b` (n = 2), folded.
async fn seeded<S: BlobStore>(e: &Engine<S>) {
    e.write("idx", vec![doc("a", 1), doc("b", 2)])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

fn n_is(n: i64) -> Predicate {
    Predicate::Cmp("n".to_owned(), Op::Eq, Value::Int(n))
}

fn set_n(id: &str, n: i64) -> Patch {
    Patch {
        id: id.to_owned(),
        set: BTreeMap::from([("n".to_owned(), Value::Int(n))]),
        unset: vec![],
    }
}

#[tokio::test]
async fn applied_counts_are_what_the_fold_decided() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    seeded(&e).await;
    let op = OperationId(1);
    let mark = e.mark().await.unwrap();
    applying(op, async {
        // An upsert of a new id and of an existing one: two rows written.
        e.write("idx", vec![doc("c", 3), doc("a", 10)])
            .await
            .unwrap();
        // b has n = 2: the first condition holds, the second does not.
        e.write_if("idx", vec![doc("b", 20)], Metric::DotProduct, &n_is(2))
            .await
            .unwrap();
        e.write_if("idx", vec![doc("b", 30)], Metric::DotProduct, &n_is(2))
            .await
            .unwrap();
        // A real patch, a patch to the same value, and a patch of nothing.
        e.patch(
            "idx",
            vec![set_n("c", 4), set_n("a", 10), set_n("zz", 1)],
            None,
        )
        .await
        .unwrap();
        // A delete of a present id and of an absent one.
        e.delete("idx", vec!["c".to_owned(), "nope".to_owned()])
            .await
            .unwrap();
    })
    .await;
    e.flush().await.unwrap();
    let got = known(e.await_applied(mark, op).await.unwrap());
    assert_eq!(
        got,
        Outcome {
            upserted: 3,
            deleted: 1,
            patched: 1,
            skipped: 4,
            rejected: 0,
        }
    );
    // And by filter: every row with n >= 10 (a and b) is patched, then deleted.
    let op = OperationId(2);
    let mark = e.mark().await.unwrap();
    let big = Predicate::Cmp("n".to_owned(), Op::Gte, Value::Int(10));
    applying(op, async {
        e.patch_by_filter("idx", &big, set_n("ignored", 99))
            .await
            .unwrap();
        e.delete_by_filter("idx", &big).await.unwrap();
        e.delete_by_filter("idx", &n_is(12345)).await.unwrap();
    })
    .await;
    e.flush().await.unwrap();
    let got = known(e.await_applied(mark, op).await.unwrap());
    assert_eq!(
        got,
        Outcome {
            upserted: 0,
            deleted: 2,
            patched: 2,
            skipped: 1,
            rejected: 0,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusive_conditions_apply_once() {
    let store = Arc::new(MemoryStore::new());
    let engines: Vec<Arc<Engine<MemoryStore>>> = (1..=4)
        .map(|l| Arc::new(Engine::new(Arc::clone(&store), T, LaneId(l))))
        .collect();
    seeded(&engines[0]).await;
    let unclaimed = Predicate::Absent("owner".to_owned());
    // Every claim is flushed before any is folded, so the first fold judges many at once.
    let flushed = Arc::new(tokio::sync::Barrier::new(100));
    let mut tasks = Vec::new();
    for i in 0..100u64 {
        let e = Arc::clone(&engines[(i % 4) as usize]);
        let (cond, flushed) = (unclaimed.clone(), Arc::clone(&flushed));
        tasks.push(tokio::spawn(async move {
            let op = OperationId(u128::from(i) + 1000);
            let mut row = doc("claim", 0);
            row.attrs.insert("owner".to_owned(), Value::Int(i as i64));
            let mark = e.mark().await.unwrap();
            applying(op, e.write_if("idx", vec![row], Metric::DotProduct, &cond))
                .await
                .unwrap();
            e.flush().await.unwrap();
            flushed.wait().await;
            (i, known(e.await_applied(mark, op).await.unwrap()))
        }));
    }
    let mut winners = Vec::new();
    for t in tasks {
        let (i, o) = t.await.unwrap();
        match o {
            Outcome {
                upserted: 1,
                skipped: 0,
                ..
            } => winners.push(i),
            Outcome {
                upserted: 0,
                skipped: 1,
                ..
            } => {}
            other => panic!("writer {i}: {other:?}"),
        }
    }
    assert_eq!(winners.len(), 1, "winners: {winners:?}");
    engines[0].fold().await.unwrap();
    let rows: Vec<Document> = engines[0]
        .segment_rows_for_test("idx")
        .await
        .unwrap()
        .into_iter()
        .flatten()
        .filter(|r| r.id == "claim")
        .collect();
    let owner = rows.last().unwrap().attrs.get("owner").cloned();
    assert_eq!(owner, Some(Value::Int(winners[0] as i64)));
}

#[tokio::test]
async fn a_request_folded_elsewhere_sums_its_outcomes() {
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), T, LaneId(1));
    let b = Engine::new(Arc::clone(&store), T, LaneId(2));
    seeded(&b).await;
    let op = OperationId(7);
    let mark = a.mark().await.unwrap();
    // Half of the request is flushed and folded by another engine; the rest by a second
    // commit. Neither commit alone holds the whole answer.
    applying(op, a.write("idx", vec![doc("x", 1)]))
        .await
        .unwrap();
    a.flush().await.unwrap();
    b.fold().await.unwrap();
    applying(op, a.delete("idx", vec!["a".to_owned()]))
        .await
        .unwrap();
    a.flush().await.unwrap();
    b.fold().await.unwrap();
    let got = known(a.await_applied(mark, op).await.unwrap());
    assert_eq!((got.upserted, got.deleted), (1, 1), "{got:?}");
}

#[tokio::test]
async fn a_cas_loser_reads_the_winners_outcome() {
    let store = Watched::new();
    let a = Engine::new(Arc::clone(&store), T, LaneId(1));
    seeded(&a).await;
    let op = OperationId(8);
    let mark = a.mark().await.unwrap();
    applying(op, a.delete("idx", vec!["a".to_owned(), "gone".to_owned()]))
        .await
        .unwrap();
    a.flush().await.unwrap();
    store.overtake.store(true, Ordering::SeqCst);
    let got = known(a.await_applied(mark, op).await.unwrap());
    assert_eq!((got.deleted, got.skipped), (1, 1), "{got:?}");
}

#[tokio::test]
async fn req_is_never_sealed() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    seeded(&e).await;
    let op = OperationId(9);
    let mark = e.mark().await.unwrap();
    applying(op, async {
        e.write("idx", vec![doc("p", 1)]).await.unwrap();
        e.patch("idx", vec![set_n("a", 5)], None).await.unwrap();
    })
    .await;
    e.flush().await.unwrap();
    e.await_applied(mark, op).await.unwrap();
    for seg in e.segment_rows_for_test("idx").await.unwrap() {
        for r in seg {
            assert!(
                r.attrs.keys().all(|k| !k.starts_with('$')),
                "{} sealed with {:?}",
                r.id,
                r.attrs.keys().collect::<Vec<_>>()
            );
        }
    }
}

/// One applied write and its fold on `e`, returning its outcome.
async fn one<S: BlobStore>(e: &Engine<S>, op: u128, id: &str) -> Applied {
    let op = OperationId(op);
    let mark = e.mark().await.unwrap();
    applying(op, e.write("idx", vec![doc(id, 1)]))
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.await_applied(mark, op).await.unwrap()
}

#[tokio::test]
async fn outcome_list_is_bounded_and_reaped() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    let first = {
        one(&e, 1, "r0").await;
        e.head_for_test().await.outcomes.first().unwrap().1.clone()
    };
    for i in 1..=OUTCOMES_KEPT {
        one(&e, i as u128 + 1, &format!("r{i}")).await;
        assert!(e.head_for_test().await.outcomes.len() <= OUTCOMES_KEPT);
    }
    let head = e.head_for_test().await;
    assert_eq!(head.outcomes.len(), OUTCOMES_KEPT);
    assert!(head.outcomes.iter().all(|(_, k)| *k != first));
    // Listed objects survive any GC; the retired one is reaped.
    assert!(store.get(&Key::new(first.clone())).await.is_ok());
    e.gc(0).await.unwrap();
    assert!(
        store.get(&Key::new(first)).await.is_err(),
        "retired outcome not reaped"
    );
    for (_, k) in &e.head_for_test().await.outcomes {
        assert!(
            store.get(&Key::new(k.clone())).await.is_ok(),
            "{k} reaped while listed"
        );
    }
}

#[tokio::test]
async fn a_retired_outcome_is_unavailable() {
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), T, LaneId(1));
    let b = Engine::new(Arc::clone(&store), T, LaneId(2));
    let op = OperationId(77);
    let mark = a.mark().await.unwrap();
    applying(op, a.write("idx", vec![doc("mine", 1)]))
        .await
        .unwrap();
    a.flush().await.unwrap();
    // The first of these folds `mine` too; the rest push its commit off the list.
    for i in 0..=OUTCOMES_KEPT {
        one(&b, 1000 + i as u128, &format!("theirs{i}")).await;
    }
    assert_eq!(
        a.await_applied(mark, op).await.unwrap(),
        Applied::Unavailable
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_applied_writes_share_a_fold() {
    let store = Watched::new();
    let e = Arc::new(Engine::new(Arc::clone(&store), T, LaneId(1)));
    seeded(&e).await;
    let before = store.head_commits.load(Ordering::SeqCst);
    let reads_before = store.head_reads.load(Ordering::SeqCst);
    let flushed = Arc::new(tokio::sync::Barrier::new(32));
    let mut tasks = Vec::new();
    for i in 0..32u128 {
        let (e, flushed) = (Arc::clone(&e), Arc::clone(&flushed));
        tasks.push(tokio::spawn(async move {
            let op = OperationId(i + 500);
            let mark = e.mark().await.unwrap();
            applying(op, e.write("idx", vec![doc(&format!("c{i}"), 1)]))
                .await
                .unwrap();
            e.flush().await.unwrap();
            flushed.wait().await;
            known(e.await_applied(mark, op).await.unwrap())
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().upserted, 1);
    }
    let folds = store.head_commits.load(Ordering::SeqCst) - before;
    assert!(
        folds <= 2,
        "{folds} HEAD commit attempts for 32 applied writes"
    );
    // A fold reads HEAD once an attempt; a waiter a fold already covered reads nothing.
    let reads = store.head_reads.load(Ordering::SeqCst) - reads_before;
    assert!(reads <= 4, "{reads} HEAD reads for 32 applied writes");
}

#[tokio::test]
async fn applied_cost_is_one_outcome_put_per_commit() {
    let store = Watched::new();
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    seeded(&e).await;
    assert_eq!(
        store.outcome_puts.load(Ordering::SeqCst),
        0,
        "an unstamped fold"
    );
    // Three requests in one commit: one outcome object.
    let mut marks = Vec::new();
    for _ in 0..3 {
        marks.push(e.mark().await.unwrap());
    }
    for i in 0..3u128 {
        applying(
            OperationId(i + 1),
            e.write("idx", vec![doc(&format!("k{i}"), 1)]),
        )
        .await
        .unwrap();
    }
    e.flush().await.unwrap();
    for (i, m) in marks.into_iter().enumerate() {
        let o = known(
            e.await_applied(m, OperationId(i as u128 + 1))
                .await
                .unwrap(),
        );
        assert_eq!(o.upserted, 1);
    }
    assert_eq!(store.outcome_puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_rows_are_counted() {
    // Two writers create one index at once, so neither door has a schema to check against.
    // Lane 1's row sets the width; lane 2's applied row is wider, and the reject pass
    // discards it.
    let store = Arc::new(MemoryStore::new());
    let one = Engine::new(Arc::clone(&store), T, LaneId(1));
    let two = Engine::new(Arc::clone(&store), T, LaneId(2));
    one.write("idx", vec![Document::new("a", vec![1.0, 0.5])])
        .await
        .unwrap();
    one.flush().await.unwrap();
    let op = OperationId(3);
    let mark = two.mark().await.unwrap();
    applying(
        op,
        two.write(
            "idx",
            vec![
                Document::new("wide", vec![1.0, 0.5, 2.0]),
                Document::new("wider", vec![1.0, 0.5, 2.0]),
            ],
        ),
    )
    .await
    .unwrap();
    two.flush().await.unwrap();
    let got = known(two.await_applied(mark, op).await.unwrap());
    assert_eq!((got.upserted, got.rejected), (0, 2), "{got:?}");
}

#[tokio::test]
async fn a_row_set_aside_after_resolve_is_rejected() {
    // multi.rs's `a_patch_beside_a_new_width_never_stops_the_fold`, applied: the patch merges
    // into a base row whose `late` disagrees with this fold's first of it, and is set aside.
    fn late(id: &str, width: usize) -> Document {
        let mut d = Document::new(id, vec![1.0, 0.5, 0.25, 0.125]);
        d.vectors.insert(
            "late".into(),
            pstore_format::VectorField::Dense(vec![vec![0.5; width]]),
        );
        d
    }
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![late("a", 3)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.commit_head_for_test(|h| {
        for s in h.schemas.values_mut() {
            s.fields.clear();
        }
    })
    .await
    .unwrap();
    let other = Engine::new(Arc::clone(&store), T, LaneId(0));
    other.write("docs", vec![late("x", 4)]).await.unwrap();
    other.flush().await.unwrap();
    let op = OperationId(61);
    let mark = e.mark().await.unwrap();
    applying(op, e.patch("docs", vec![set_n("a", 7)], None))
        .await
        .unwrap();
    e.flush().await.unwrap();
    let got = known(e.await_applied(mark, op).await.unwrap());
    assert_eq!((got.patched, got.rejected), (0, 1), "{got:?}");
}

/// Criterion 7: one applied write's cost, recorded rather than bounded (it is an opt-in
/// outside D-34's depth budget), except LIST, which is zero.
#[tokio::test]
async fn applied_write_cost_is_recorded() {
    use pstore_blob::{Accounted, OpClass};
    use pstore_testkit::depth::DepthCounting;
    let depth = DepthCounting::new(MemoryStore::new());
    let acct = Accounted::new(depth.clone());
    let e = Engine::new(Arc::new(acct.as_tenant(T)), T, LaneId(1));
    seeded(&e).await;
    let counts = || [OpClass::Write, OpClass::Read, OpClass::List].map(|c| acct.count(T, c));
    for (label, applied) in [("durable", false), ("applied", true)] {
        let before = counts();
        depth.reset();
        let op = OperationId(4242);
        if applied {
            let mark = e.mark().await.unwrap();
            applying(op, e.write("idx", vec![doc("m", 1)]))
                .await
                .unwrap();
            e.flush().await.unwrap();
            known(e.await_applied(mark, op).await.unwrap());
        } else {
            e.write("idx", vec![doc("d", 1)]).await.unwrap();
            e.flush().await.unwrap();
        }
        let after = counts();
        let [w, r, l] = [0, 1, 2].map(|i| after[i] - before[i]);
        println!(
            "{label}: W {w}, R {r}, LIST {l}, sequential depth {}, requests {}",
            depth.depth(),
            depth.requests()
        );
        assert_eq!(l, 0, "{label} listed");
    }
}

/// Code review of M129: an idle engine's burst of applied writes refreshes its stale view
/// of HEAD once, not once a request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_after_idle_reads_head_once_for_its_marks() {
    let store = Watched::new();
    let e = Arc::new(Engine::new(Arc::clone(&store), T, LaneId(1)));
    seeded(&e).await;
    tokio::time::sleep(pstore_engine::MARK_FRESH + std::time::Duration::from_millis(50)).await;
    let before = store.head_reads.load(Ordering::SeqCst);
    let marks: Vec<_> = (0..32)
        .map(|_| {
            let e = Arc::clone(&e);
            tokio::spawn(async move { e.mark().await.unwrap() })
        })
        .collect();
    for m in marks {
        m.await.unwrap();
    }
    let reads = store.head_reads.load(Ordering::SeqCst) - before;
    assert_eq!(reads, 1, "{reads} HEAD reads for 32 marks");
}

/// Code review of M129: waiters whose request another engine folded share one read of
/// that engine's outcome object.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiters_share_the_read_of_anothers_outcome() {
    let store = Watched::new();
    let e = Arc::new(Engine::new(Arc::clone(&store), T, LaneId(1)));
    seeded(&e).await;
    let flushed = Arc::new(tokio::sync::Barrier::new(33));
    let mut tasks = Vec::new();
    for i in 0..32u128 {
        let (e, flushed) = (Arc::clone(&e), Arc::clone(&flushed));
        tasks.push(tokio::spawn(async move {
            let op = OperationId(i + 900);
            let mark = e.mark().await.unwrap();
            applying(op, e.write("idx", vec![doc(&format!("w{i}"), 1)]))
                .await
                .unwrap();
            e.flush().await.unwrap();
            flushed.wait().await;
            // The rival has folded everything before any waiter looks.
            flushed.wait().await;
            known(e.await_applied(mark, op).await.unwrap())
        }));
    }
    flushed.wait().await;
    store.rival.fold().await.unwrap();
    flushed.wait().await;
    for t in tasks {
        assert_eq!(t.await.unwrap().upserted, 1);
    }
    let reads = store.outcome_reads.load(Ordering::SeqCst);
    assert_eq!(reads, 1, "{reads} reads of one outcome object");
}
