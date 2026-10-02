//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M23: a segment is created, never replaced. Two engines share one lane and one store; the
//! rows they fold are written by a third on another lane, so M17's `LaneTaken` never enters.
//! A pause between a HEAD read and a segment write is made by holding one engine's next
//! write of a kind until the test releases it.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Engine, EngineError, Head};
use pstore_format::Document;
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{CasTag, LaneId, TenantId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const T: TenantId = TenantId(2300);

/// What a test asks of one engine's view of the shared store.
#[derive(Debug, Default)]
struct Knobs {
    /// Hold the next write whose key ends with this, until `release`.
    hold: Mutex<Option<&'static str>>,
    reached: Notify,
    release: Notify,
    /// Refuse every create of a key ending with this, with `Lost`.
    refuse: Mutex<Option<&'static str>>,
    /// Answer the next HEAD CAS with `Contended`, writing nothing.
    contend_head: AtomicBool,
    refused: AtomicU32,
    /// Conditional writes of a segment or delete vector that were created.
    created: AtomicU32,
    /// Unconditional writes of a segment or delete vector.
    replaced: AtomicU32,
    /// Every read and every write, of any kind, through this view.
    reads: AtomicU32,
    writes: AtomicU32,
}

#[derive(Debug, Clone)]
struct Store {
    inner: Arc<MemoryStore>,
    knobs: Arc<Knobs>,
}

impl Store {
    /// A view of `inner` with knobs of its own.
    fn view(inner: &Arc<MemoryStore>) -> Self {
        Self {
            inner: Arc::clone(inner),
            knobs: Arc::default(),
        }
    }
    async fn held(&self, key: &Key) {
        let hit = {
            let mut h = self.knobs.hold.lock().unwrap();
            match *h {
                Some(s) if key.as_str().ends_with(s) => h.take().is_some(),
                _ => false,
            }
        };
        if hit {
            self.knobs.reached.notify_one();
            self.knobs.release.notified().await;
        }
    }
}

fn object(key: &Key) -> bool {
    let k = key.as_str();
    k.contains("/seg/") && (k.ends_with(".seg") || k.ends_with(".dv"))
}

#[async_trait::async_trait]
impl BlobStore for Store {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.knobs.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.knobs.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.knobs.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.knobs.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.knobs.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.knobs.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.knobs.writes.fetch_add(1, Ordering::SeqCst);
        self.held(key).await;
        if object(key) {
            self.knobs.replaced.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.knobs.writes.fetch_add(1, Ordering::SeqCst);
        self.held(key).await;
        if key.as_str().ends_with("/HEAD") && self.knobs.contend_head.swap(false, Ordering::SeqCst)
        {
            return Err(CasError::Contended);
        }
        let refuse = *self.knobs.refuse.lock().unwrap();
        if refuse.is_some_and(|s| key.as_str().ends_with(s)) && pre == Precondition::NotExists {
            self.knobs.refused.fetch_add(1, Ordering::SeqCst);
            return Err(CasError::Lost);
        }
        let out = self.inner.put_conditional(key, body, pre).await;
        if object(key) && out.is_ok() {
            self.knobs.created.fetch_add(1, Ordering::SeqCst);
        }
        out
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

fn doc(id: &str, x: f32) -> Document {
    Document::new(id, vec![x, 1.0, 0.5])
}

fn engine(store: &Store, lane: u64) -> Engine<Store> {
    Engine::new(Arc::new(store.clone()), T, LaneId(lane))
}

/// A writer on lane 9, and two folders sharing lane 1, each with knobs of its own.
struct World {
    inner: Arc<MemoryStore>,
    w: Engine<Store>,
    a: Engine<Store>,
    b: Engine<Store>,
    sa: Store,
}

impl World {
    fn new() -> Self {
        let inner = Arc::new(MemoryStore::new());
        let sa = Store::view(&inner);
        Self {
            w: engine(&Store::view(&inner), 9),
            a: engine(&sa, 1),
            b: engine(&Store::view(&inner), 1),
            sa,
            inner,
        }
    }
    async fn put(&self, index: &str, ids: &[&str]) {
        let docs = ids.iter().zip(1..).map(|(i, n)| doc(i, n as f32)).collect();
        self.w.write(index, docs).await.unwrap();
        self.w.flush().await.unwrap();
    }
    async fn bytes(&self, key: &str) -> Option<Bytes> {
        self.inner.get(&Key::new(key.to_owned())).await.ok()
    }
    async fn objects(&self) -> BTreeMap<String, Bytes> {
        let mut out = BTreeMap::new();
        for k in self
            .inner
            .list_unrestricted(&Key::new(String::new()))
            .await
            .unwrap()
        {
            let b = self.inner.get(&k).await.unwrap();
            out.insert(k.as_str().to_owned(), b);
        }
        out
    }
    async fn ids(&self, index: &str) -> BTreeSet<String> {
        self.w
            .scan(index, None)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect()
    }
    async fn head(&self) -> Head {
        self.w.head_for_test().await
    }
    /// Runs `a` held at its next write ending with `kind`, with `meanwhile` run in the pause.
    async fn paused<T>(
        &self,
        kind: &'static str,
        a: impl std::future::Future<Output = T>,
        meanwhile: impl std::future::Future<Output = ()>,
    ) -> T {
        *self.sa.knobs.hold.lock().unwrap() = Some(kind);
        let knobs = Arc::clone(&self.sa.knobs);
        let (got, ()) = tokio::join!(a, async {
            knobs.reached.notified().await;
            meanwhile.await;
            knobs.release.notify_one();
        });
        got
    }
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

fn segments(h: &Head, index: &str) -> Vec<String> {
    h.indexes
        .get(index)
        .map(|v| v.iter().map(|r| r.key.clone()).collect())
        .unwrap_or_default()
}

fn graveyard(h: &Head) -> BTreeSet<String> {
    h.graveyard.values().flatten().cloned().collect()
}

#[tokio::test]
async fn a_paused_fold_does_not_replace_a_live_segment() {
    let w = World::new();
    w.put("idx", &["a"]).await;
    let got = w
        .paused(".seg", w.a.fold(), async {
            // B folds what A read and more, and commits at the key A is about to write.
            w.put("idx", &["b"]).await;
            w.b.fold().await.unwrap();
        })
        .await;
    got.unwrap();
    let head = w.head().await;
    let live = segments(&head, "idx");
    assert_eq!(live.len(), 1, "{live:?}");
    let before = w.bytes(&live[0]).await;
    assert_eq!(
        w.ids("idx").await,
        set(&["a", "b"]),
        "B's rows were replaced"
    );
    // Nothing A did after its release touched B's segment.
    w.a.fold().await.unwrap();
    assert_eq!(w.bytes(&live[0]).await, before);
}

#[tokio::test]
async fn a_paused_fold_does_not_replace_a_live_delete_vector() {
    let w = World::new();
    w.put("idx", &["a", "b", "c"]).await;
    w.w.fold().await.unwrap();
    w.w.delete("idx", vec!["a".into()]).await.unwrap();
    w.w.flush().await.unwrap();
    let got = w
        .paused(".dv", w.a.fold(), async {
            w.w.delete("idx", vec!["b".into()]).await.unwrap();
            w.w.flush().await.unwrap();
            w.b.fold().await.unwrap();
        })
        .await;
    got.unwrap();
    assert_eq!(
        w.ids("idx").await,
        set(&["c"]),
        "B's delete vector was replaced"
    );
}

#[tokio::test]
async fn a_paused_compaction_does_not_replace_a_live_segment() {
    // A fixes its inputs, loses its CAS to a fold on another lane, and is held at its
    // re-seal for the next epoch. B, on A's lane, merges the newer HEAD at that very key.
    // Two compactions of one HEAD seal identical bytes, so only the re-seal can differ.
    let w = World::new();
    for id in ["a", "b"] {
        w.put("idx", &[id]).await;
        w.w.fold().await.unwrap();
    }
    let knobs = Arc::clone(&w.sa.knobs);
    let (got, ()) = tokio::join!(
        w.a.compact_with_interference_for_test("idx", async {
            w.put("idx", &["c"]).await;
            w.w.fold().await.unwrap();
            *knobs.hold.lock().unwrap() = Some(".seg");
        }),
        async {
            knobs.reached.notified().await;
            w.b.compact("idx").await.unwrap().unwrap();
            knobs.release.notify_one();
        }
    );
    // A's inputs are gone from HEAD: it discards.
    assert_eq!(got.unwrap(), None);
    assert_eq!(
        w.ids("idx").await,
        set(&["a", "b", "c"]),
        "B's merge was replaced"
    );
}

#[tokio::test]
async fn a_branch_does_not_replace_a_live_delete_vector() {
    // `two` is branched from `one` and deletes another row, so both carry a vector for the
    // same segment with different rows. Two same-lane branches to `dest` copy them to one key.
    let w = World::new();
    w.put("one", &["a", "b", "c"]).await;
    w.w.fold().await.unwrap();
    w.w.branch("one", "two").await.unwrap();
    w.w.delete("one", vec!["a".into()]).await.unwrap();
    w.w.delete("two", vec!["b".into()]).await.unwrap();
    w.w.flush().await.unwrap();
    w.w.fold().await.unwrap();
    let got = w
        .paused(".dv", w.a.branch("one", "dest"), async {
            w.b.branch("two", "dest").await.unwrap();
        })
        .await;
    // A finds `dest` taken on its retry.
    assert!(got.is_err(), "{got:?}");
    assert_eq!(
        w.ids("dest").await,
        set(&["a", "c"]),
        "B's copied vector was replaced"
    );
}

#[tokio::test]
async fn a_fold_retried_at_its_own_epoch_takes_the_next_name() {
    let w = World::new();
    w.put("idx", &["a"]).await;
    w.sa.knobs.contend_head.store(true, Ordering::SeqCst);
    let before = w.objects().await;
    w.a.fold().await.unwrap();
    let head = w.head().await;
    let live = segments(&head, "idx");
    assert_eq!(live.len(), 1);
    assert!(live[0].ends_with("_1.seg"), "{live:?}");
    // The first attempt's object is still there, never replaced, and is buried (criterion 11).
    let first = live[0].replace("_1.seg", ".seg");
    assert!(
        w.bytes(&first).await.is_some(),
        "the first attempt's segment is gone"
    );
    assert!(!before.contains_key(&first));
    assert!(
        graveyard(&head).contains(&first),
        "the discarded name was not buried"
    );
    assert_eq!(w.sa.knobs.replaced.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_compaction_retried_at_its_own_epoch_does_not_reseal() {
    let w = World::new();
    for id in ["a", "b"] {
        w.put("idx", &[id]).await;
        w.w.fold().await.unwrap();
    }
    // An object already at the merge's first name, so ours is `_1`; then HEAD contends once.
    let epoch = w.head().await.epoch.0 + 1;
    let first = format!(
        "{:04x}/tnt/{}/idx/idx/seg/L1/{epoch:020}-{:016x}.seg",
        T.0 as u16, T.0, 1
    );
    w.inner
        .put(&Key::new(first.clone()), Bytes::from_static(b"planted"))
        .await
        .unwrap();
    w.sa.knobs.contend_head.store(true, Ordering::SeqCst);
    w.a.compact("idx").await.unwrap().unwrap();
    let live = segments(&w.head().await, "idx");
    assert_eq!(live, [format!("{}_1.seg", first.trim_end_matches(".seg"))]);
    // One merged segment created, not one per attempt.
    assert_eq!(w.sa.knobs.created.load(Ordering::SeqCst), 1);
    assert_eq!(
        w.bytes(&first).await.unwrap(),
        Bytes::from_static(b"planted")
    );
}

/// Reads and writes `op` makes through A's view.
async fn cost<T>(w: &World, op: impl std::future::Future<Output = T>) -> (T, u32, u32) {
    let k = &w.sa.knobs;
    let (r, wr) = (
        k.reads.load(Ordering::SeqCst),
        k.writes.load(Ordering::SeqCst),
    );
    let out = op.await;
    (
        out,
        k.reads.load(Ordering::SeqCst) - r,
        k.writes.load(Ordering::SeqCst) - wr,
    )
}

#[tokio::test]
async fn an_uncontended_fold_compaction_and_branch_are_unchanged() {
    let w = World::new();
    w.put("idx", &["a"]).await;
    let (_, fr, fw) = cost(&w, w.a.fold()).await;
    w.put("idx", &["b"]).await;
    w.w.fold().await.unwrap();
    w.w.delete("idx", vec!["a".into()]).await.unwrap();
    w.w.flush().await.unwrap();
    let (_, dr, dw) = cost(&w, w.a.fold()).await;
    w.put("idx", &["c"]).await;
    w.w.fold().await.unwrap();
    let (_, cr, cw) = cost(&w, w.a.compact("idx")).await;
    w.w.delete("idx", vec!["b".into()]).await.unwrap();
    w.w.flush().await.unwrap();
    w.w.fold().await.unwrap();
    let (_, br, bw) = cost(&w, w.a.branch("idx", "copy")).await;
    // ⚠️ Exactly what the same operations cost before M23 (measured on the parent commit, code
    // review round 1): a create is the PUT it replaced, never a probe before it.
    assert_eq!((fr, fw), (11, 2), "a fold");
    assert_eq!((dr, dw), (15, 2), "a fold that deletes");
    assert_eq!((cr, cw), (8, 2), "a compaction");
    assert_eq!((br, bw), (2, 2), "a branch copying a delete vector");
    // Today's names, no suffix anywhere, and nothing written unconditionally.
    let head = w.head().await;
    let named: Vec<String> = head
        .indexes
        .values()
        .flatten()
        .map(|r| r.key.clone())
        .chain(head.deletes.values().map(|(k, _)| k.clone()))
        .collect();
    assert!(!named.is_empty());
    for k in &named {
        let stem = k.rsplit('/').next().unwrap();
        assert!(!stem.contains('_'), "{k} carries a suffix");
    }
    assert_eq!(w.sa.knobs.replaced.load(Ordering::SeqCst), 0);
    assert_eq!(w.sa.knobs.refused.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_refused_name_costs_exactly_one_write() {
    // The fold's first name is taken by an object no HEAD names: one more write, no read.
    let w = World::new();
    w.put("idx", &["a"]).await;
    let epoch = w.head().await.epoch.0 + 1;
    let taken = format!(
        "{:04x}/tnt/{}/idx/idx/seg/L0/{epoch:020}-{:016x}.seg",
        T.0 as u16, T.0, 1
    );
    w.inner
        .put(&Key::new(taken), Bytes::from_static(b"taken"))
        .await
        .unwrap();
    let (got, r, wr) = cost(&w, w.a.fold()).await;
    got.unwrap();
    assert_eq!((r, wr), (11, 3));
    assert!(segments(&w.head().await, "idx")[0].ends_with("_1.seg"));
}

#[tokio::test]
async fn a_segment_claims_its_name_before_its_sidecars() {
    // A writes a centroid table for every segment; B, by default, for none below 25,000 rows.
    let inner = Arc::new(MemoryStore::new());
    let sa = Store::view(&inner);
    let params = pstore_index::cluster::Params {
        exact_scan_threshold: 1,
        ..pstore_index::cluster::Params::default()
    };
    let w = World {
        w: engine(&Store::view(&inner), 9),
        a: engine(&sa, 1).with_index_params(params),
        b: engine(&Store::view(&inner), 1),
        sa,
        inner,
    };
    w.put("idx", &["a", "b", "c", "d"]).await;
    let got = w
        .paused(".seg", w.a.fold(), async {
            w.put("idx", &["e"]).await;
            w.b.fold().await.unwrap();
        })
        .await;
    got.unwrap();
    let live = segments(&w.head().await, "idx");
    assert_eq!(live.len(), 1);
    let cen = format!("{}.cen", live[0]);
    assert!(
        w.bytes(&cen).await.is_none(),
        "B's segment gained A's centroid table"
    );
    assert_eq!(w.ids("idx").await, set(&["a", "b", "c", "d", "e"]));
}

#[tokio::test]
async fn a_refused_name_is_never_buried() {
    // A planted object at the merge's first name, which no HEAD names, so burial's live
    // filter cannot be what keeps it.
    let w = World::new();
    for id in ["a", "b"] {
        w.put("idx", &[id]).await;
        w.w.fold().await.unwrap();
    }
    let epoch = w.head().await.epoch.0 + 1;
    let planted = format!(
        "{:04x}/tnt/{}/idx/idx/seg/L1/{epoch:020}-{:016x}.seg",
        T.0 as u16, T.0, 1
    );
    w.inner
        .put(&Key::new(planted.clone()), Bytes::from_static(b"not ours"))
        .await
        .unwrap();
    // Another lane's compaction wins, so A discards and buries what it wrote.
    let other = engine(&Store::view(&w.inner), 2);
    let got =
        w.a.compact_with_interference_for_test("idx", async {
            other.compact("idx").await.unwrap().unwrap();
        })
        .await
        .unwrap();
    assert_eq!(got, None);
    let grave = graveyard(&w.head().await);
    assert!(!grave.contains(&planted), "the refused name was buried");
    assert!(
        grave.iter().any(|k| k.ends_with("_1.seg")),
        "A's own merge was not buried: {grave:?}"
    );
    assert_eq!(
        w.bytes(&planted).await.unwrap(),
        Bytes::from_static(b"not ours")
    );
}

#[tokio::test]
async fn sixteen_refusals_fail_the_operation() {
    let w = World::new();
    for id in ["a", "b"] {
        w.put("idx", &[id]).await;
        w.w.fold().await.unwrap();
    }
    w.put("idx", &["c"]).await;
    *w.sa.knobs.refuse.lock().unwrap() = Some(".seg");
    let err =
        w.a.fold()
            .await
            .expect_err("a fold sealed past every refusal");
    assert!(matches!(err, EngineError::Contended), "{err:?}");
    assert_eq!(w.sa.knobs.refused.load(Ordering::SeqCst), 16);
    w.sa.knobs.refused.store(0, Ordering::SeqCst);
    let err =
        w.a.compact("idx")
            .await
            .expect_err("a compaction sealed past every refusal");
    assert!(matches!(err, EngineError::Contended), "{err:?}");
    assert_eq!(w.sa.knobs.refused.load(Ordering::SeqCst), 16);
    assert_eq!(w.sa.knobs.replaced.load(Ordering::SeqCst), 0);
}

/// A's segments, created since `before`.
async fn mine(w: &World, before: &BTreeSet<String>) -> Vec<String> {
    w.objects()
        .await
        .into_keys()
        .filter(|k| !before.contains(k) && k.ends_with(".seg") && k.contains("-0000000000000001"))
        .collect()
}

#[tokio::test]
async fn a_fold_buries_what_its_discarded_attempts_created() {
    // A lost CAS: another lane folds everything between A's seal and A's commit.
    let w = World::new();
    w.put("idx", &["a"]).await;
    let other = engine(&Store::view(&w.inner), 2);
    let before: BTreeSet<String> = w.objects().await.into_keys().collect();
    let epochs = w.head().await.epoch.0;
    let got = w
        .paused(".seg", w.a.fold(), async {
            other.write("idx", vec![doc("z", 9.0)]).await.unwrap();
            other.flush().await.unwrap();
            other.fold().await.unwrap();
            // A row only A's retry can fold, so the retry commits.
            w.put("idx", &["b"]).await;
        })
        .await;
    got.unwrap();
    let head = w.head().await;
    // Two commits, the other lane's and A's retry: no burial commit of its own.
    assert_eq!(head.epoch.0, epochs + 2);
    let live: BTreeSet<String> = head
        .indexes
        .values()
        .flatten()
        .map(|r| r.key.clone())
        .collect();
    let grave = graveyard(&head);
    let mine = mine(&w, &before).await;
    assert_eq!(mine.len(), 2, "{mine:?}");
    for k in &mine {
        assert!(live.contains(k) || grave.contains(k), "{k} leaked");
    }
    assert!(
        mine.iter().any(|k| grave.contains(k)),
        "nothing was buried: {mine:?}"
    );
    w.w.gc(0).await.unwrap();
    for k in mine.iter().filter(|k| grave.contains(*k)) {
        assert!(w.bytes(k).await.is_none(), "{k} survived GC");
    }
    assert_eq!(w.ids("idx").await, set(&["a", "b", "z"]));
}

#[tokio::test]
async fn a_fold_left_with_nothing_buries_at_its_next_commit() {
    // The other lane folds everything, so A's retry has nothing to commit and its seal waits
    // for A's next fold commit.
    let w = World::new();
    w.put("idx", &["a"]).await;
    let other = engine(&Store::view(&w.inner), 2);
    let before: BTreeSet<String> = w.objects().await.into_keys().collect();
    let got = w
        .paused(".seg", w.a.fold(), async {
            other.fold().await.unwrap();
        })
        .await;
    got.unwrap();
    let left = mine(&w, &before).await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(!graveyard(&w.head().await).contains(&left[0]));
    w.put("idx", &["b"]).await;
    w.a.fold().await.unwrap();
    assert!(
        graveyard(&w.head().await).contains(&left[0]),
        "the next commit did not bury {}",
        left[0]
    );
    // Buried once and forgotten: reaped, it is never buried again by a later commit.
    w.w.gc(0).await.unwrap();
    assert!(!graveyard(&w.head().await).contains(&left[0]));
    w.put("idx", &["c"]).await;
    w.a.fold().await.unwrap();
    assert!(
        !graveyard(&w.head().await).contains(&left[0]),
        "{} was buried again after it was reaped",
        left[0]
    );
}

#[tokio::test]
async fn a_suffixed_name_is_resolved_reaped_and_not_a_copy() {
    // A `_1` segment, live: the past reads it. Compacted away: GC reaps it with its sidecars.
    let w = World::new();
    w.put("idx", &["a"]).await;
    w.sa.knobs.contend_head.store(true, Ordering::SeqCst);
    let at = w.a.fold().await.unwrap();
    let key = segments(&w.head().await, "idx")[0].clone();
    assert!(key.ends_with("_1.seg"), "{key}");
    w.put("idx", &["b"]).await;
    w.w.fold().await.unwrap();
    w.w.compact("idx").await.unwrap().unwrap();
    // The past reads the suffixed segment: `a`, its only row, at the epoch it was committed.
    let dense = vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 1.0, 0.5],
        limit: 5,
        tune: Query::default(),
    }];
    let then =
        w.w.query_as_of("idx", at, &dense, Fusion::default(), 5)
            .await
            .unwrap();
    assert_eq!(then.hits.len(), 1, "the past lost the suffixed segment");
    assert!(graveyard(&w.head().await).contains(&key));
    w.w.gc(0).await.unwrap();
    let left: Vec<String> = w
        .objects()
        .await
        .into_keys()
        .filter(|k| k.starts_with(key.trim_end_matches(".seg")))
        .collect();
    assert!(left.is_empty(), "GC left {left:?}");
}
