//! Idempotency keys for applied writes (M132): of the attempts carrying one key, at most one
//! applies; a later one is answered with the winner's outcome, and one the fold cannot prove
//! is not a repeat is refused. See `mutation-completion.md` §9.

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
    Applied, Consistency, Engine, IdempotencyKey, OUTCOMES_KEPT, OperationId, Outcome, applying,
    keyed,
};
use pstore_format::{Document, Value};
use pstore_query::OrderBy;
use pstore_types::{CasTag, Epoch, LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const T: TenantId = TenantId(132);

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

fn engine(store: &Arc<MemoryStore>, lane: u64) -> Engine<MemoryStore> {
    let e = Engine::new(Arc::clone(store), T, LaneId(lane));
    e.set_idempotency(true);
    e
}

/// One keyed attempt: `op` upserts `id = n` under `key`, then waits for its answer.
async fn attempt<S: BlobStore>(
    e: &Engine<S>,
    key: &IdempotencyKey,
    op: u128,
    id: &str,
    n: i64,
) -> Applied {
    let op = OperationId(op);
    let mark = e.mark().await.unwrap();
    keyed(op, key, false, e.write("idx", vec![doc(id, n)]))
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.await_keyed(mark, op, key).await.unwrap()
}

/// One unkeyed applied write: a commit that tallies, and so takes a place in HEAD's list.
async fn tallying<S: BlobStore>(e: &Engine<S>, op: u128, id: &str) {
    let op = OperationId(op);
    let mark = e.mark().await.unwrap();
    applying(op, e.write("other", vec![doc(id, 1)]))
        .await
        .unwrap();
    e.flush().await.unwrap();
    assert!(matches!(
        e.await_applied(mark, op).await.unwrap(),
        Applied::Known(..)
    ));
}

/// Every row of `idx`, as `id -> n`.
async fn rows<S: BlobStore>(e: &Engine<S>) -> BTreeMap<String, i64> {
    let by_id = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    e.ordered_as("idx", &by_id, None, 0, 1000, None, Consistency::Eventual)
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|d| {
            let n = match d.attrs.get("n") {
                Some(Value::Int(n)) => *n,
                other => panic!("{other:?}"),
            };
            (d.id, n)
        })
        .collect()
}

fn own(a: &Applied) -> Outcome {
    match a {
        Applied::Known(o, _) => *o,
        other => panic!("not its own outcome: {other:?}"),
    }
}

fn replayed(a: &Applied) -> Outcome {
    match a {
        Applied::Replayed(Some((o, _))) => *o,
        other => panic!("not a replay with counts: {other:?}"),
    }
}

/// Criterion 1: a retry after a lost response is answered with the first attempt's outcome,
/// and does not apply; criterion 4: so is one served by a new engine.
#[tokio::test]
async fn a_retry_is_answered_not_applied() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, 1);
    let key = e.idempotency_key(7).await.unwrap();
    // The response is lost.
    let first = attempt(&e, &key, 10, "x", 1).await;
    assert_eq!(own(&first).upserted, 1);
    e.write("idx", vec![doc("x", 2)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let retry = attempt(&e, &key, 11, "x", 1).await;
    assert_eq!(replayed(&retry).upserted, 1, "{retry:?}");
    assert_eq!(rows(&e).await["x"], 2);
    // Criterion 4: a new engine, caches empty, answers the same.
    let fresh = engine(&store, 3);
    let again = attempt(&fresh, &key, 12, "x", 1).await;
    assert_eq!(replayed(&again).upserted, 1, "{again:?}");
    assert_eq!(rows(&fresh).await["x"], 2);
}

/// Criterion 2: two attempts folded by one fold; the first in fold order wins.
#[tokio::test]
async fn one_winner_in_one_fold() {
    let store = Arc::new(MemoryStore::new());
    let (a, b) = (engine(&store, 1), engine(&store, 2));
    let key = a.idempotency_key(8).await.unwrap();
    let (ma, mb) = (a.mark().await.unwrap(), b.mark().await.unwrap());
    keyed(
        OperationId(1),
        &key,
        false,
        a.write("idx", vec![doc("a", 1)]),
    )
    .await
    .unwrap();
    keyed(
        OperationId(2),
        &key,
        false,
        b.write("idx", vec![doc("b", 1)]),
    )
    .await
    .unwrap();
    a.flush().await.unwrap();
    b.flush().await.unwrap();
    a.fold().await.unwrap();
    let ra = a.await_keyed(ma, OperationId(1), &key).await.unwrap();
    let rb = b.await_keyed(mb, OperationId(2), &key).await.unwrap();
    let ids: Vec<String> = rows(&a).await.into_keys().collect();
    assert_eq!(ids, ["a"], "{ra:?} {rb:?}");
    assert_eq!(own(&ra).upserted, 1);
    assert_eq!(replayed(&rb), own(&ra));
}

/// Criterion 3: 100 attempts of one key over 4 engines, concurrently: one applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_once_under_contention() {
    let store = Arc::new(MemoryStore::new());
    let engines: Vec<Arc<Engine<MemoryStore>>> =
        (1..=4).map(|l| Arc::new(engine(&store, l))).collect();
    let key = engines[0].idempotency_key(9).await.unwrap();
    let before = engines[0].head_for_test().await.epoch;
    let mut tasks = Vec::new();
    for i in 0..100u64 {
        let e = Arc::clone(&engines[(i % 4) as usize]);
        tasks.push(tokio::spawn(async move {
            attempt(&e, &key, 1000 + u128::from(i), &format!("c{i}"), 1).await
        }));
    }
    let mut answers = Vec::new();
    for t in tasks {
        answers.push(t.await.unwrap());
    }
    let commits = engines[0].head_for_test().await.epoch.0 - before.0;
    println!("{commits} commits for 100 attempts");
    assert_eq!(rows(&engines[0]).await.len(), 1);
    let winners: Vec<&Applied> = answers
        .iter()
        .filter(|a| matches!(a, Applied::Known(..)))
        .collect();
    assert_eq!(winners.len(), 1, "{answers:?}");
    let won = own(winners[0]);
    for a in &answers {
        match a {
            Applied::Known(o, _) => assert_eq!(*o, won),
            Applied::Replayed(Some((o, _))) => assert_eq!(*o, won),
            Applied::Replayed(None) => {
                assert!(
                    commits >= OUTCOMES_KEPT as u64,
                    "unavailable after {commits}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}

/// Criterion 5: a retry decided while a listed record names the winner is replayed -- with
/// counts while the winner's commit is still listed after the retry's own commit, without
/// them once that commit drops it -- and refused once no listed record names it.
#[tokio::test]
async fn expiry_is_a_refusal_at_the_boundary() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, 1);
    let key = e.idempotency_key(5).await.unwrap();
    assert_eq!(key.e0(), 0);
    attempt(&e, &key, 1, "x", 1).await;
    assert_eq!(
        e.head_for_test().await.epoch,
        Epoch(1),
        "winner not at e0 + 1"
    );
    for i in 0..(OUTCOMES_KEPT as u128 - 2) {
        tallying(&e, 100 + i, &format!("t{i}")).await;
    }
    // Its own commit is the 32nd listed: the winner's stays.
    let retry = attempt(&e, &key, 2, "late", 1).await;
    assert_eq!(replayed(&retry).upserted, 1, "{retry:?}");
    // Decided against a HEAD that lists the winner; its own commit drops it.
    let retry = attempt(&e, &key, 3, "late", 1).await;
    assert_eq!(retry, Applied::Replayed(None));
    // No listed record names the winner any more.
    for i in 0..OUTCOMES_KEPT as u128 {
        tallying(&e, 200 + i, &format!("u{i}")).await;
    }
    let refused = attempt(&e, &key, 4, "late", 1).await;
    assert_eq!(refused, Applied::Expired);
    assert!(!rows(&e).await.contains_key("late"));
}

/// Criterion 6: a key left unused through 32 tallying commits still applies; through 33 it
/// is refused, and applies nothing.
#[tokio::test]
async fn a_stale_key_never_applies() {
    for (commits, applies) in [(OUTCOMES_KEPT, true), (OUTCOMES_KEPT + 1, false)] {
        let store = Arc::new(MemoryStore::new());
        let e = engine(&store, 1);
        let key = e.idempotency_key(6).await.unwrap();
        assert_eq!(key.e0(), 0);
        for i in 0..commits as u128 {
            tallying(&e, 100 + i, &format!("t{i}")).await;
        }
        let got = attempt(&e, &key, 1, "x", 1).await;
        if applies {
            assert_eq!(own(&got).upserted, 1, "{commits}: {got:?}");
        } else {
            assert_eq!(got, Applied::Expired, "{commits}");
        }
        assert_eq!(rows(&e).await.contains_key("x"), applies, "{commits}");
    }
}

/// Criterion 7: one attempt whose second flush comes 32 tallying commits after its first
/// is refused as partial, though its window fell off the list.
#[tokio::test]
async fn partial_is_reported() {
    let store = Arc::new(MemoryStore::new());
    let (e, b) = (engine(&store, 1), engine(&store, 2));
    let key = e.idempotency_key(4).await.unwrap();
    let op = OperationId(1);
    let mark = e.mark().await.unwrap();
    keyed(op, &key, false, e.write("idx", vec![doc("p", 1)]))
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    for i in 0..OUTCOMES_KEPT as u128 {
        tallying(&b, 100 + i, &format!("t{i}")).await;
    }
    keyed(op, &key, false, e.write("idx", vec![doc("q", 1)]))
        .await
        .unwrap();
    e.flush().await.unwrap();
    assert_eq!(
        e.await_keyed(mark, op, &key).await.unwrap(),
        Applied::Partial
    );
    let ids: Vec<String> = rows(&e).await.into_keys().collect();
    assert_eq!(ids, ["p"]);
}

/// Criterion 9: a HEAD re-committed as a pre-M132 binary would, without `keys_since` or a
/// list, expires a key that won; it never re-admits it.
#[tokio::test]
async fn a_head_recommit_expires_never_readmits() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, 1);
    let key = e.idempotency_key(3).await.unwrap();
    attempt(&e, &key, 1, "x", 1).await;
    assert!(e.head_for_test().await.keys_since.is_some());
    e.commit_head_for_test(|h| {
        h.keys_since = None;
        h.outcomes.clear();
    })
    .await
    .unwrap();
    let retry = attempt(&e, &key, 2, "late", 1).await;
    assert_eq!(retry, Applied::Expired);
    assert!(!rows(&e).await.contains_key("late"));
}

/// Criterion 11: an engine not set for keys still raises `keys_since` when it drops an
/// entry, so the winner's forgotten key is refused, not applied again.
#[tokio::test]
async fn raising_is_not_the_flags() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, 1);
    let plain = Engine::new(Arc::clone(&store), T, LaneId(2));
    let key = e.idempotency_key(2).await.unwrap();
    attempt(&e, &key, 1, "x", 1).await;
    for i in 0..OUTCOMES_KEPT as u128 {
        tallying(&plain, 100 + i, &format!("t{i}")).await;
    }
    // From an engine whose mark is fresh: `e`'s is a second old at most, and 32 commits
    // past it, so its window fell off and it could only answer "may be partly applied".
    let retry = attempt(&engine(&store, 3), &key, 2, "late", 1).await;
    assert_eq!(retry, Applied::Expired);
    assert!(!rows(&e).await.contains_key("late"));
}

/// Criterion 8, engine side: a key ahead of HEAD is refused at the door, even after a fresh
/// HEAD read; one at or below it is admitted.
#[tokio::test]
async fn a_key_ahead_of_head_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, 1);
    tallying(&e, 1, "t").await;
    assert!(e.admit_key(&IdempotencyKey::new(1, 1)).await.unwrap());
    assert!(!e.admit_key(&IdempotencyKey::new(2, 1)).await.unwrap());
    assert!(
        !e.admit_key(&IdempotencyKey::new(u64::MAX, 1))
            .await
            .unwrap()
    );
    // Another engine moved HEAD: a key from it is admitted after one fresh read.
    let other = engine(&store, 2);
    tallying(&other, 2, "u").await;
    assert!(e.admit_key(&IdempotencyKey::new(2, 1)).await.unwrap());
}

/// The key's spelling round-trips, and anything else is refused.
#[test]
fn a_key_parses_only_as_issued() {
    let k = IdempotencyKey::new(42, 0xab);
    let s = k.to_string();
    assert_eq!(s, format!("42-{:032x}", 0xab));
    assert_eq!(IdempotencyKey::parse(&s), Some(k));
    for bad in [
        "",
        "42",
        "42-",
        "-00000000000000000000000000000000",
        "x-00000000000000000000000000000000",
        "42-0000000000000000000000000000000",
        "42-000000000000000000000000000000000",
        "42-0000000000000000000000000000000G",
        "42-0000000000000000000000000000000A",
        "+42-00000000000000000000000000000000",
        "042-00000000000000000000000000000000",
        "18446744073709551616-00000000000000000000000000000000",
    ] {
        assert_eq!(IdempotencyKey::parse(bad), None, "{bad:?}");
    }
}

/// §9.2: an engine set for keys initialises `keys_since` on any commit it makes, a
/// compaction as well as a fold; one that is not never does, so below level 3 HEAD keeps
/// M131's bytes.
#[tokio::test]
async fn only_a_flagged_engine_initialises_keys_since() {
    let store = Arc::new(MemoryStore::new());
    let plain = Engine::new(Arc::clone(&store), T, LaneId(2));
    for n in 0..2 {
        plain
            .write("idx", vec![doc(&format!("r{n}"), 1)])
            .await
            .unwrap();
        plain.flush().await.unwrap();
        plain.fold().await.unwrap();
    }
    tallying(&plain, 1, "t").await;
    let before = plain.head_for_test().await;
    assert_eq!(before.keys_since, None);
    let e = engine(&store, 1);
    e.compact("idx").await.unwrap().expect("nothing compacted");
    assert_eq!(e.head_for_test().await.keys_since, Some(before.epoch.0));
}

/// The next outcome object read answers "not found" when armed, as one a newer HEAD dropped
/// and GC reaped would.
struct Vanishing {
    inner: Arc<MemoryStore>,
    armed: AtomicBool,
}

#[async_trait::async_trait]
impl BlobStore for Vanishing {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        if key.as_str().contains("/out/") && self.armed.swap(false, Ordering::SeqCst) {
            return Err(BlobError::NotFound(key.as_str().to_owned()));
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

/// §9.3: a listed object the fold cannot find makes it read HEAD again, never decide as if
/// the object named no winner.
#[tokio::test]
async fn a_vanished_outcome_is_read_again_not_ignored() {
    let inner = Arc::new(MemoryStore::new());
    let e = engine(&inner, 1);
    let key = e.idempotency_key(1).await.unwrap();
    attempt(&e, &key, 1, "x", 1).await;
    let store = Arc::new(Vanishing {
        inner,
        armed: AtomicBool::new(true),
    });
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(2));
    fresh.set_idempotency(true);
    let retry = attempt(&fresh, &key, 2, "late", 1).await;
    assert_eq!(replayed(&retry).upserted, 1, "{retry:?}");
    assert!(!store.armed.load(Ordering::SeqCst), "nothing vanished");
    assert!(!rows(&fresh).await.contains_key("late"));
}
