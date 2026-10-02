//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M25: a row a fold rejects is set aside in a quarantine HEAD names, never dropped. A cold
//! process -- one that never read HEAD -- writes past the door, as M7d's `schema.rs` does.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_engine::{Engine, Head, Metric};
use pstore_format::{Document, Value};
use pstore_types::{Epoch, LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

const T: TenantId = TenantId(2500);

type Store = pstore_blob::TenantView<MemoryStore>;

fn doc(id: &str, dims: usize) -> Document {
    let mut d = Document::new(id, (0..dims).map(|i| 1.0 + i as f32).collect());
    d.attrs
        .insert("n".to_owned(), Value::Str(format!("of {id}")));
    d
}

struct World {
    acct: Arc<Accounted<MemoryStore>>,
    store: Arc<Store>,
    first: Engine<Store>,
    cold: Engine<Store>,
}

impl World {
    /// `docs` created at width 4 by `first`; `cold` has never read HEAD.
    async fn new() -> Self {
        let acct = Arc::new(Accounted::new(MemoryStore::new()));
        let store = Arc::new(acct.as_tenant(T));
        let first = Engine::new(Arc::clone(&store), T, LaneId(1));
        first.write("docs", vec![doc("a", 4)]).await.unwrap();
        first.flush().await.unwrap();
        first.fold().await.unwrap();
        let cold = Engine::new(Arc::clone(&store), T, LaneId(2));
        Self {
            acct,
            store,
            first,
            cold,
        }
    }
    /// Two wrong-width rows in `docs`, one right one in `other`, folded by `cold`.
    async fn reject(&self) -> Epoch {
        self.cold
            .write_without_schema_check_for_test("docs", vec![doc("w1", 2), doc("w2", 2)])
            .await;
        self.cold
            .write("other", vec![doc("fine", 3)])
            .await
            .unwrap();
        self.cold
            .flush_without_schema_check_for_test()
            .await
            .unwrap();
        self.cold.fold().await.unwrap()
    }
    async fn head(&self) -> Head {
        self.first.head_for_test().await
    }
    async fn exists(&self, key: &str) -> bool {
        self.store.head(&Key::new(key.to_owned())).await.is_ok()
    }
}

fn quarantined(h: &Head, index: &str) -> Vec<String> {
    h.quarantine
        .get(index)
        .map(|v| v.iter().map(|(k, _)| k.clone()).collect())
        .unwrap_or_default()
}

fn graveyard(h: &Head) -> BTreeSet<String> {
    h.graveyard.values().flatten().cloned().collect()
}

fn ids(rows: &[pstore_engine::Quarantined]) -> BTreeSet<String> {
    rows.iter().map(|r| r.document.id.clone()).collect()
}

#[tokio::test]
async fn a_rejected_row_is_quarantined_intact() {
    let w = World::new().await;
    w.reject().await;
    let head = w.head().await;
    assert_eq!(head.schema_rejects.get("docs").copied(), Some(2));
    let q = w.first.quarantine("docs").await.unwrap().unwrap();
    assert_eq!(ids(&q.rows), ["w1", "w2"].map(String::from).into());
    for r in &q.rows {
        let want = doc(&r.document.id, 2);
        assert_eq!(r.document.vector(), want.vector(), "{}", r.document.id);
        assert_eq!(r.document.attrs, want.attrs, "{}", r.document.id);
        assert!(r.reason.is_some(), "no reason for {}", r.document.id);
    }
    assert_eq!(q.epoch, head.epoch);
}

#[tokio::test]
async fn a_euclidean_row_is_exported_in_client_space() {
    // A row rejected for its metric: the index is dot product, this writer euclidean.
    let w = World::new().await;
    w.cold
        .write_as("docs", vec![doc("e", 4)], Metric::EuclideanSquared)
        .await
        .unwrap();
    w.cold.flush_without_schema_check_for_test().await.unwrap();
    w.cold.fold().await.unwrap();
    let q = w.first.quarantine("docs").await.unwrap().unwrap();
    assert_eq!(q.rows.len(), 1, "{:?}", w.head().await.schema_rejects);
    let r = &q.rows[0];
    assert_eq!(
        r.document.vector(),
        doc("e", 4).vector(),
        "the stored norm component was kept"
    );
    assert!(r.reserved.contains_key("$metric"), "{:?}", r.reserved);
    assert!(r.document.attrs.keys().all(|k| !k.starts_with('$')));
}

#[tokio::test]
async fn a_quarantine_is_never_buried_while_named() {
    let w = World::new().await;
    w.reject().await;
    let head = w.head().await;
    let keys = quarantined(&head, "docs");
    assert_eq!(keys.len(), 1);
    assert!(
        !graveyard(&head).contains(&keys[0]),
        "buried in the commit that names it"
    );
    // Later folds, a compaction and a branch bury nothing of it either.
    w.first.write("docs", vec![doc("b", 4)]).await.unwrap();
    w.first.flush().await.unwrap();
    w.first.fold().await.unwrap();
    w.first.compact("docs").await.unwrap();
    w.first.branch("docs", "copy").await.unwrap();
    assert!(!graveyard(&w.head().await).contains(&keys[0]));
}

#[tokio::test]
async fn gc_spares_a_named_quarantine() {
    let w = World::new().await;
    w.reject().await;
    let key = quarantined(&w.head().await, "docs")[0].clone();
    // Buried by hand while named, long past any window: GC must still keep it.
    let k = key.clone();
    w.first
        .commit_head_for_test(|h| h.graveyard.entry(1).or_default().push(k))
        .await
        .unwrap();
    w.first.gc(0).await.unwrap();
    assert!(w.exists(&key).await, "GC reaped a quarantine HEAD names");
}

#[tokio::test]
async fn a_quarantine_survives_gc() {
    let w = World::new().await;
    w.reject().await;
    let before = w.first.quarantine("docs").await.unwrap().unwrap();
    w.first.gc(0).await.unwrap();
    let after = w.first.quarantine("docs").await.unwrap().unwrap();
    assert_eq!(ids(&after.rows), ids(&before.rows));
    assert_eq!(after.rows.len(), 2);
}

#[tokio::test]
async fn discard_buries_exactly_what_was_exported() {
    let w = World::new().await;
    w.reject().await;
    let seen = w.first.quarantine("docs").await.unwrap().unwrap();
    let first_key = quarantined(&w.head().await, "docs")[0].clone();
    // A second rejecting fold after the export: not seen, so not discarded.
    w.cold
        .write_without_schema_check_for_test("docs", vec![doc("w3", 2)])
        .await;
    w.cold.flush_without_schema_check_for_test().await.unwrap();
    w.cold.fold().await.unwrap();
    let n = w
        .first
        .discard_quarantine("docs", seen.epoch)
        .await
        .unwrap();
    assert_eq!(n, Some(1));
    let head = w.head().await;
    assert_eq!(quarantined(&head, "docs").len(), 1);
    assert_eq!(
        ids(&w.first.quarantine("docs").await.unwrap().unwrap().rows),
        ["w3".to_owned()].into()
    );
    // Buried at the discard's epoch, so a retention window keeps it until it passes.
    let at = head
        .graveyard
        .iter()
        .find(|(_, ks)| ks.contains(&first_key))
        .map(|(e, _)| *e);
    assert_eq!(at, Some(head.epoch.0), "not buried at the discard's epoch");
    w.first.gc(1).await.unwrap();
    assert!(
        w.exists(&first_key).await,
        "reaped inside its retention window"
    );
    w.first.gc(0).await.unwrap();
    assert!(!w.exists(&first_key).await, "never reaped");
    // Nothing left that the export showed: no commit.
    let e = w.head().await.epoch;
    assert_eq!(
        w.first
            .discard_quarantine("docs", seen.epoch)
            .await
            .unwrap(),
        Some(0)
    );
    assert_eq!(w.head().await.epoch, e);
}

#[tokio::test]
async fn dropping_an_index_buries_its_quarantine() {
    let w = World::new().await;
    w.reject().await;
    let key = quarantined(&w.head().await, "docs")[0].clone();
    w.first.delete_index("docs").await.unwrap();
    let head = w.head().await;
    assert!(!head.quarantine.contains_key("docs"));
    let at = head
        .graveyard
        .iter()
        .find(|(_, ks)| ks.contains(&key))
        .map(|(e, _)| *e);
    assert_eq!(at, Some(head.epoch.0));
    w.first.gc(0).await.unwrap();
    assert!(!w.exists(&key).await);
}

#[tokio::test]
async fn an_index_known_only_by_its_rejects_is_found() {
    // HEAD records `docs`' schema and names none of its segments -- the state a fold leaves
    // when every row it holds for an index is rejected -- and then a rejecting fold.
    let w = World::new().await;
    w.first
        .commit_head_for_test(|h| {
            h.indexes.remove("docs");
        })
        .await
        .unwrap();
    w.reject().await;
    let head = w.head().await;
    assert!(
        !head.indexes.contains_key("docs"),
        "the setup sealed something"
    );
    let q = w.first.quarantine("docs").await.unwrap().unwrap();
    assert_eq!(ids(&q.rows), ["w1", "w2"].map(String::from).into());
    // And it exists for its metadata and the listing (code review round 1, M1).
    let stats = w
        .first
        .index_stats("docs")
        .await
        .unwrap()
        .expect("no stats for it");
    assert_eq!((stats.segments, stats.documents), (0, 0));
    assert_eq!(stats.quarantined_rows, 2);
    assert!(
        w.first
            .indexes()
            .await
            .unwrap()
            .contains(&"docs".to_owned())
    );
    assert!(w.first.quarantine("nope").await.unwrap().is_none());
    assert!(
        w.first
            .discard_quarantine("nope", q.epoch)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_rejecting_fold_costs_one_write_per_index() {
    let w = World::new().await;
    w.cold
        .write_without_schema_check_for_test("docs", vec![doc("w1", 2)])
        .await;
    w.cold.write("other", vec![doc("fine", 3)]).await.unwrap();
    w.cold.flush_without_schema_check_for_test().await.unwrap();
    let (r0, w0) = (
        w.acct.count(T, OpClass::Read),
        w.acct.count(T, OpClass::Write),
    );
    w.cold.fold().await.unwrap();
    let (r1, w1) = (
        w.acct.count(T, OpClass::Read),
        w.acct.count(T, OpClass::Write),
    );
    // ⚠️ Measured on the parent commit, the same fold: (19 reads, 2 writes). One more write
    // -- the quarantine for `docs`, beside `other`'s segment and the HEAD CAS -- and no read.
    assert_eq!((r1 - r0, w1 - w0), (19, 3));
}
