//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Abandoned work buries what it wrote (M19): a compaction or a branch that gives up after
//! writing objects commits them to the graveyard under their own key epochs -- never a key the
//! HEAD it commits still names -- so GC reaps them and the past is unchanged.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Engine, EngineError, Head};
use pstore_format::Document;
use pstore_query::OrderBy;
use pstore_types::{CasTag, Epoch, LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const T: TenantId = TenantId(190);

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

/// A memory store, shared by its clones, that can refuse the next segment write.
#[derive(Debug, Default, Clone)]
struct Store {
    inner: MemoryStore,
    refuse_seal: Arc<AtomicBool>,
    /// The next segment create waits for `go`, then answers `Io` having written nothing (M23).
    io_seal: Arc<IoSeal>,
    head_reads: Arc<std::sync::atomic::AtomicU64>,
    head_cas: Arc<std::sync::atomic::AtomicU64>,
    /// HEAD commits to let through before answering one `Contended`; `u64::MAX` for never.
    contend_after: Arc<std::sync::atomic::AtomicU64>,
    /// Whether every HEAD commit from now on answers `Lost`.
    lose_all: Arc<AtomicBool>,
}

/// An `Io` create that did not land, with a pause before it is answered.
#[derive(Debug, Default)]
struct IoSeal {
    armed: AtomicBool,
    reached: tokio::sync::Notify,
    go: tokio::sync::Notify,
}

impl Store {
    fn new() -> Self {
        let s = Self::default();
        s.contend_after.store(u64::MAX, Ordering::SeqCst);
        s
    }

    fn counts(&self) -> (u64, u64) {
        (
            self.head_reads.load(Ordering::SeqCst),
            self.head_cas.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl BlobStore for Store {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
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
        // M23: a segment is created, never replaced, so its faults are a create's.
        if key.as_str().ends_with(".seg") && self.refuse_seal.swap(false, Ordering::SeqCst) {
            return Err(CasError::Io("the store is down".to_owned()));
        }
        if key.as_str().ends_with(".seg") && self.io_seal.armed.swap(false, Ordering::SeqCst) {
            self.io_seal.reached.notify_one();
            self.io_seal.go.notified().await;
            return Err(CasError::Io("timed out".to_owned()));
        }
        if key.as_str().ends_with("/HEAD") {
            self.head_cas.fetch_add(1, Ordering::SeqCst);
            if self.lose_all.load(Ordering::SeqCst) {
                return Err(CasError::Lost);
            }
            let left = self.contend_after.load(Ordering::SeqCst);
            if left == 0 {
                self.contend_after.store(u64::MAX, Ordering::SeqCst);
                return Err(CasError::Contended);
            }
            if left != u64::MAX {
                self.contend_after.store(left - 1, Ordering::SeqCst);
            }
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
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_immutable(key, class).await
    }
}

fn engine(store: &Store, lane: u64) -> Engine<Store> {
    Engine::new(Arc::new(store.clone()), T, LaneId(lane))
}

async fn fold(e: &Engine<Store>) -> Epoch {
    e.flush().await.unwrap();
    e.fold().await.unwrap()
}

/// `index` with a segment per id.
async fn seeded(e: &Engine<Store>, index: &str, ids: &[&str]) {
    for id in ids {
        e.write(index, vec![doc(id)]).await.unwrap();
        fold(e).await;
    }
}

/// Every object in the store.
async fn objects(store: &Store) -> BTreeSet<String> {
    store
        .inner
        .list_unrestricted(&Key::new(String::new()))
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.as_str().to_owned())
        .collect()
}

/// Every key HEAD names: segments, and delete vectors.
fn named(h: &Head) -> BTreeSet<String> {
    h.indexes
        .values()
        .flatten()
        .map(|r| r.key.clone())
        .chain(h.deletes.values().map(|(k, _)| k.clone()))
        .collect()
}

/// The epoch a graveyard entry for `key` sits at, if any.
fn buried_at(h: &Head, key: &str) -> Option<u64> {
    h.graveyard
        .iter()
        .find(|(_, keys)| keys.iter().any(|k| k == key))
        .map(|(e, _)| *e)
}

/// A key's own epoch: the first number of its file name.
fn key_epoch(key: &str) -> u64 {
    let file = key.rsplit('/').next().unwrap();
    let digits: String = file.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap()
}

/// Every row id of `index` as of each epoch in `1..=upto`, or `None` where it did not exist.
async fn history(e: &Engine<Store>, index: &str, upto: u64) -> Vec<Option<Vec<String>>> {
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let mut out = Vec::new();
    for ep in 1..=upto {
        let o = e
            .ordered(index, &by, None, 0, 10_000, Some(Epoch(ep)))
            .await
            .unwrap();
        out.push(o.exists.then(|| o.rows.into_iter().map(|d| d.id).collect()));
    }
    out
}

/// The past as another process saw it, captured before a burial could change it.
type Past = Arc<Mutex<Option<(u64, Vec<Option<Vec<String>>>)>>>;

/// What an abandonment left: the objects it wrote that HEAD does not name.
async fn orphans_of(store: &Store, before: &BTreeSet<String>, h: &Head) -> Vec<String> {
    let named = named(h);
    objects(store)
        .await
        .into_iter()
        .filter(|k| !before.contains(k) && !named.contains(k))
        .filter(|k| k.ends_with(".seg") || k.ends_with(".dv"))
        .collect()
}

/// The shape every abandonment test ends with: what was written and left unnamed is buried at
/// its own key epoch, the past answers as it did before the burial, and GC takes it all.
async fn buried_and_reaped(
    store: &Store,
    e: &Engine<Store>,
    index: &str,
    before: &BTreeSet<String>,
    past: &Past,
) {
    let head = e.head_for_test().await;
    let left = orphans_of(store, before, &head).await;
    assert!(!left.is_empty(), "the fixture wrote nothing to bury");
    for k in &left {
        let at = buried_at(&head, k).unwrap_or_else(|| panic!("{k} was not buried"));
        let own = if k.ends_with(".dv") {
            k.rsplit('.')
                .nth(1)
                .unwrap()
                .split('-')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        } else {
            key_epoch(k)
        };
        assert_eq!(at, own, "{k} buried at {at}, not its own epoch {own}");
    }
    let (upto, then) = past.lock().unwrap().clone().expect("the interference ran");
    assert_eq!(history(e, index, upto).await, then, "the past changed");
    e.gc(0).await.unwrap();
    let after = objects(store).await;
    for k in &left {
        assert!(
            !after.iter().any(|o| o.starts_with(k.as_str())),
            "{k} or its sidecars survived GC"
        );
    }
}

async fn capture(e: &Engine<Store>, index: &str, past: &Past) {
    let upto = e.head_for_test().await.epoch.0;
    *past.lock().unwrap() = Some((upto, history(e, index, upto).await));
}

#[tokio::test]
async fn a_discarded_compaction_is_buried() {
    let store = Store::new();
    let (e, w) = (engine(&store, 1), engine(&store, 2));
    seeded(&e, "idx", &["a", "b", "c"]).await;
    let before = objects(&store).await;
    let past: Past = Arc::default();
    let got = e
        .compact_with_interference_for_test("idx", async {
            w.compact("idx").await.unwrap().unwrap();
            capture(&w, "idx", &past).await;
        })
        .await
        .unwrap();
    assert_eq!(got, None);
    buried_and_reaped(&store, &e, "idx", &before, &past).await;
    let live = e.scan("idx", None).await.unwrap();
    assert_eq!(live.len(), 3);
}

#[tokio::test]
async fn a_compaction_whose_delete_vector_moved_is_buried() {
    let store = Store::new();
    let (e, w) = (engine(&store, 1), engine(&store, 2));
    seeded(&e, "idx", &["a", "b", "c"]).await;
    let before = objects(&store).await;
    let past: Past = Arc::default();
    let got = e
        .compact_with_interference_for_test("idx", async {
            w.delete("idx", vec!["b".into()]).await.unwrap();
            fold(&w).await;
            capture(&w, "idx", &past).await;
        })
        .await
        .unwrap();
    assert_eq!(got, None);
    buried_and_reaped(&store, &e, "idx", &before, &past).await;
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_refused_branch_retry_is_buried() {
    let store = Store::new();
    let (e, w) = (engine(&store, 1), engine(&store, 2));
    seeded(&e, "src", &["a", "b"]).await;
    e.delete("src", vec!["a".into()]).await.unwrap();
    fold(&e).await;
    let before = objects(&store).await;
    let past: Past = Arc::default();
    let err = e
        .branch_with_interference_for_test("src", "dest", async {
            w.write("dest", vec![doc("mine")]).await.unwrap();
            fold(&w).await;
            capture(&w, "dest", &past).await;
        })
        .await
        .expect_err("the retry was not refused");
    assert!(matches!(err, EngineError::Refused(_)), "{err:?}");
    buried_and_reaped(&store, &e, "dest", &before, &past).await;
    let ids: Vec<String> = e
        .scan("dest", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, ["mine"]);
}

#[tokio::test]
async fn a_same_lane_winners_key_is_not_buried() {
    // M23: a refused name is never recorded, so the one way a recorded name is live is an
    // `Io` create that did not land, whose name the same-lane twin then won and committed.
    // The twin runs inside the store's pause, before ours is answered `Io`.
    let store = Store::new();
    let (e, twin) = (engine(&store, 1), engine(&store, 1));
    seeded(&e, "idx", &["a", "b"]).await;
    store.io_seal.armed.store(true, Ordering::SeqCst);
    let (got, ()) = tokio::join!(e.compact("idx"), async {
        store.io_seal.reached.notified().await;
        twin.compact("idx").await.unwrap().unwrap();
        store.io_seal.go.notify_one();
    });
    assert!(matches!(got, Err(EngineError::Blob(_))), "{got:?}");
    let head = e.head_for_test().await;
    let live = named(&head);
    assert!(
        live.iter().any(|k| k.contains("/seg/L1/")),
        "the twin did not commit"
    );
    for k in live {
        assert_eq!(buried_at(&head, &k), None, "the live {k} was buried");
    }
    e.gc(0).await.unwrap();
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_same_lane_winners_copies_are_not_buried() {
    let store = Store::new();
    let (e, twin) = (engine(&store, 1), engine(&store, 1));
    seeded(&e, "src", &["a", "b"]).await;
    e.delete("src", vec!["a".into()]).await.unwrap();
    fold(&e).await;
    e.branch_with_interference_for_test("src", "dest", async {
        twin.branch("src", "dest").await.unwrap();
    })
    .await
    .expect_err("the retry was not refused");
    let head = e.head_for_test().await;
    for k in named(&head) {
        assert_eq!(buried_at(&head, &k), None, "the live {k} was buried");
    }
    e.gc(0).await.unwrap();
    let ids: Vec<String> = e
        .scan("dest", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, ["b"]);
}

#[tokio::test]
async fn an_error_after_sealing_buries_the_seal() {
    // Another index's fold moves the epoch, so the retry re-seals at a new key -- and that
    // PUT fails. The first seal is buried, and the error is what the call returns.
    let store = Store::new();
    let (e, w) = (engine(&store, 1), engine(&store, 2));
    seeded(&e, "idx", &["a", "b"]).await;
    let before = objects(&store).await;
    let err = e
        .compact_with_interference_for_test("idx", async {
            w.write("other", vec![doc("x")]).await.unwrap();
            fold(&w).await;
            store.refuse_seal.store(true, Ordering::SeqCst);
        })
        .await
        .expect_err("the failed re-seal was not reported");
    assert!(matches!(err, EngineError::Blob(_)), "{err:?}");
    let head = e.head_for_test().await;
    let left = orphans_of(&store, &before, &head).await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(buried_at(&head, &left[0]), Some(key_epoch(&left[0])));
}

#[tokio::test]
async fn nothing_written_and_success_commit_no_burial() {
    let store = Store::new();
    let (e, w) = (engine(&store, 1), engine(&store, 2));
    // A merge of fully deleted rows seals nothing; discarded, it commits nothing more.
    seeded(&e, "gone", &["a", "b"]).await;
    e.delete("gone", vec!["a".into(), "b".into()])
        .await
        .unwrap();
    fold(&e).await;
    let winner = Arc::new(Mutex::new(Epoch(0)));
    let after = Arc::new(Mutex::new((0, 0)));
    let got = e
        .compact_with_interference_for_test("gone", async {
            *winner.lock().unwrap() = w.compact("gone").await.unwrap().unwrap();
            *after.lock().unwrap() = store.counts();
        })
        .await
        .unwrap();
    assert_eq!(got, None);
    // Its commit lost, it re-read HEAD and discarded -- and read nothing more.
    let (r, c) = *after.lock().unwrap();
    assert_eq!((store.counts().0 - r, store.counts().1 - c), (1, 1));
    assert_eq!(e.head_for_test().await.epoch, *winner.lock().unwrap());
    // A compaction that lands commits once, and reads HEAD once.
    seeded(&e, "idx", &["a", "b"]).await;
    let at = e.head_for_test().await.epoch;
    let reads = store.head_reads.load(Ordering::SeqCst);
    let landed = e.compact("idx").await.unwrap().unwrap();
    assert_eq!(
        store.head_reads.load(Ordering::SeqCst) - reads,
        1,
        "a success read HEAD again"
    );
    assert_eq!(landed, at.next());
    assert_eq!(e.head_for_test().await.epoch, landed);
}

#[tokio::test]
async fn a_key_already_buried_is_not_buried_twice() {
    // The same-lane twin wins the name ours' `Io` create did not take (M23), commits it, and
    // then compacts it away itself, burying it. Ours' burial finds it buried already.
    let store = Store::new();
    let (e, twin) = (engine(&store, 1), engine(&store, 1));
    seeded(&e, "idx", &["a", "b"]).await;
    let won: Arc<Mutex<Option<String>>> = Arc::default();
    store.io_seal.armed.store(true, Ordering::SeqCst);
    let (got, ()) = tokio::join!(e.compact("idx"), async {
        store.io_seal.reached.notified().await;
        twin.compact("idx").await.unwrap().unwrap();
        let k = twin.head_for_test().await.indexes["idx"][0].key.clone();
        *won.lock().unwrap() = Some(k);
        seeded(&twin, "idx", &["c"]).await;
        twin.compact("idx").await.unwrap().unwrap();
        store.io_seal.go.notify_one();
    });
    assert!(matches!(got, Err(EngineError::Blob(_))), "{got:?}");
    let k = won.lock().unwrap().clone().unwrap();
    let head = e.head_for_test().await;
    let entries = head
        .graveyard
        .values()
        .flatten()
        .filter(|g| **g == k)
        .count();
    assert_eq!(entries, 1, "{k} buried {entries} times");
}

#[tokio::test]
async fn an_abandonment_costs_one_read_and_one_commit() {
    // After the winner: the loser's commit loses (1 CAS), it re-reads HEAD (1 read) and
    // discards. The burial then reads HEAD once and commits once -- and a `Contended` answer
    // is retried on the HEAD it holds, costing a CAS and no read.
    for contended in [false, true] {
        let store = Store::new();
        let (e, w) = (engine(&store, 1), engine(&store, 2));
        seeded(&e, "idx", &["a", "b"]).await;
        let after = Arc::new(Mutex::new((0, 0)));
        e.compact_with_interference_for_test("idx", async {
            w.compact("idx").await.unwrap().unwrap();
            *after.lock().unwrap() = store.counts();
            if contended {
                store.contend_after.store(1, Ordering::SeqCst);
            }
        })
        .await
        .unwrap();
        let (r, c) = *after.lock().unwrap();
        let want = if contended { (2, 3) } else { (2, 2) };
        assert_eq!(
            (store.counts().0 - r, store.counts().1 - c),
            want,
            "contended: {contended}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_burial_that_keeps_losing_gives_up_after_its_last_attempt() {
    // Every commit after the winner's loses. The loser's commit loses and it re-reads HEAD
    // (1 CAS, 1 read). The burial reads HEAD, then makes its 24 attempts, re-reading after
    // each lost one but the last: a read after the last attempt is spent on nothing.
    let store = Store::new();
    let (e, w) = (engine(&store, 1), engine(&store, 2));
    seeded(&e, "idx", &["a", "b"]).await;
    let after = Arc::new(Mutex::new((0, 0)));
    let out = e
        .compact_with_interference_for_test("idx", async {
            w.compact("idx").await.unwrap().unwrap();
            *after.lock().unwrap() = store.counts();
            store.lose_all.store(true, Ordering::SeqCst);
        })
        .await
        .unwrap();
    assert!(out.is_none());
    let (r, c) = *after.lock().unwrap();
    assert_eq!(
        (store.counts().0 - r, store.counts().1 - c),
        (1 + 1 + 23, 1 + 24)
    );
}

/// Every object in the store, with its bytes.
async fn bytes_of(store: &Store) -> std::collections::BTreeMap<String, Bytes> {
    let mut out = std::collections::BTreeMap::new();
    for k in objects(store).await {
        out.insert(k.clone(), store.inner.get(&Key::new(k)).await.unwrap());
    }
    out
}

#[tokio::test]
async fn sealing_one_key_twice_writes_the_same_bytes() {
    // M20's read cache keeps a key's bytes for as long as its disk does, so no key may ever
    // change its bytes. Until M23 that held because two same-lane compactions sealed one key
    // identically and the loser's PUT landed on the winner's; now it holds because a key is
    // created once: the twin is refused ours' name and commits another.
    let store = Store::new();
    let (e, twin) = (engine(&store, 1), engine(&store, 1));
    seeded(&e, "idx", &["a", "b", "c"]).await;
    let before = objects(&store).await;
    let sealed = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let got = e
        .compact_with_interference_for_test("idx", async {
            // Ours has sealed; the twin is refused that name, seals another, and commits it.
            *sealed.lock().unwrap() = bytes_of(&store).await;
            twin.compact("idx").await.unwrap().unwrap();
        })
        .await
        .unwrap();
    assert_eq!(got, None);
    let ours = sealed.lock().unwrap().clone();
    let after = bytes_of(&store).await;
    let new: Vec<&String> = ours.keys().filter(|k| !before.contains(*k)).collect();
    assert!(
        new.iter().any(|k| k.ends_with(".seg")),
        "nothing sealed: {new:?}"
    );
    let live = named(&e.head_for_test().await);
    assert!(
        live.iter().any(|k| k.ends_with("_1.seg")),
        "the twin did not commit the next name: {live:?}"
    );
    // Every key keeps the bytes it was first written with: ours' until GC reaps them, and
    // nothing of ours is live.
    for k in new {
        assert!(!live.contains(k), "{k} is ours and live");
        if let Some(b) = after.get(k) {
            assert_eq!(Some(b), ours.get(k), "{k} changed its bytes");
        }
    }
}
