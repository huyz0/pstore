//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M149: an owner makes an index read-only. Every write door refuses it, and a fold sets
//! aside, never drops, the rows a door that had not read the flag accepted.

use pstore_blob::MemoryStore;
use pstore_engine::{Applied, Engine, EngineError, Metric, OperationId, Patch, applying};
use pstore_format::{Document, Value};
use pstore_query::{Op, Predicate};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;

const T: TenantId = TenantId(149);

type E = Engine<MemoryStore>;

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

fn engines() -> (E, E) {
    let store = Arc::new(MemoryStore::new());
    (
        Engine::new(Arc::clone(&store), T, LaneId(1)),
        Engine::new(store, T, LaneId(2)),
    )
}

async fn fold(e: &E) {
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

async fn ids(e: &E, index: &str) -> Vec<String> {
    let mut out: Vec<_> = e
        .scan(index, None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    out.sort();
    out
}

fn n_is(n: i64) -> Predicate {
    Predicate::Cmp("n".to_owned(), Op::Eq, Value::Int(n))
}

fn set_n(id: &str, n: i64) -> Patch {
    Patch {
        id: id.to_owned(),
        set: BTreeMap::from([("n".to_owned(), Value::Int(n))]),
        unset: Vec::new(),
    }
}

fn read_only(r: Result<impl std::fmt::Debug, EngineError>, index: &str) {
    match r {
        Err(EngineError::IndexReadOnly(i)) if i == index => {}
        other => panic!("not refused as read-only: {other:?}"),
    }
}

/// Every kind of write to `index`, each expected refused.
async fn refuses_every_kind(e: &E, index: &str) {
    read_only(e.write(index, vec![doc("x", 9)]).await, index);
    read_only(
        e.write_as(index, vec![doc("x", 9)], Metric::DotProduct)
            .await,
        index,
    );
    read_only(
        e.write_if(index, vec![doc("x", 9)], Metric::DotProduct, &n_is(1))
            .await,
        index,
    );
    read_only(e.patch(index, vec![set_n("a", 9)], None).await, index);
    read_only(e.delete(index, vec!["a".into()]).await, index);
    read_only(e.delete_if(index, vec!["a".into()], &n_is(1)).await, index);
    read_only(e.delete_by_filter(index, &n_is(1)).await, index);
    read_only(
        e.patch_by_filter(index, &n_is(1), set_n("", 9)).await,
        index,
    );
}

#[tokio::test]
async fn a_read_only_index_refuses_every_write_kind() {
    let (a, b) = engines();
    a.write("idx", vec![doc("a", 1), doc("b", 2)])
        .await
        .unwrap();
    fold(&a).await;
    let before = a.head_for_test().await.epoch;
    let set = a.set_read_only("idx", true).await.unwrap().unwrap();
    assert!(set > before);
    assert!(a.head_for_test().await.read_only.contains("idx"));
    // The process that set it, and one whose first write reads HEAD.
    refuses_every_kind(&a, "idx").await;
    refuses_every_kind(&b, "idx").await;
    read_only(a.delete_index("idx").await, "idx");
    // Nothing was buffered: a fold commits nothing, and the rows are as they were.
    for e in [&a, &b] {
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    assert_eq!(
        a.head_for_test().await.epoch,
        set,
        "a refused write committed"
    );
    assert_eq!(ids(&a, "idx").await, ["a", "b"]);
    let s = a.index_stats("idx").await.unwrap().unwrap();
    assert!(s.read_only);
    assert_eq!((s.rejected_rows, s.quarantined_rows), (0, 0));
    // Another index is untouched.
    a.write("other", vec![doc("o", 1)]).await.unwrap();
    fold(&a).await;
    assert_eq!(ids(&a, "other").await, ["o"]);
}

#[tokio::test]
async fn setting_the_value_it_has_commits_nothing() {
    let (a, _) = engines();
    a.write("idx", vec![doc("a", 1)]).await.unwrap();
    fold(&a).await;
    let epoch = a.head_for_test().await.epoch;
    assert_eq!(a.set_read_only("idx", false).await.unwrap(), Some(epoch));
    let on = a.set_read_only("idx", true).await.unwrap().unwrap();
    assert_eq!(a.set_read_only("idx", true).await.unwrap(), Some(on));
    assert_eq!(a.head_for_test().await.epoch, on);
    // A name HEAD does not hold, and one held only unfolded, are not flagged.
    assert_eq!(a.set_read_only("absent", true).await.unwrap(), None);
    a.write("held", vec![doc("h", 1)]).await.unwrap();
    assert_eq!(a.set_read_only("held", true).await.unwrap(), None);
    assert_eq!(a.head_for_test().await.epoch, on, "no write");
}

#[tokio::test]
async fn a_stale_door_is_set_aside() {
    let (a, b) = engines();
    a.write("idx", vec![doc("a", 1), doc("b", 2)])
        .await
        .unwrap();
    fold(&a).await;
    // `b` reads HEAD at its first write, before the flag, and accepts every kind after it.
    b.write("idx", vec![doc("c", 3)]).await.unwrap();
    a.set_read_only("idx", true).await.unwrap().unwrap();
    let op = OperationId(7);
    let mark = b.mark().await.unwrap();
    applying(op, b.write("idx", vec![doc("d", 4)]))
        .await
        .unwrap();
    b.delete("idx", vec!["a".into()]).await.unwrap();
    b.patch("idx", vec![set_n("b", 20)], None).await.unwrap();
    b.flush().await.unwrap();
    let got = b.await_applied(mark, op).await.unwrap();
    match got {
        Applied::Known(o, _) => assert_eq!((o.upserted, o.rejected), (0, 1), "{o:?}"),
        other => panic!("{other:?}"),
    }
    // Served as before; every kind kept in the quarantine and counted.
    assert_eq!(ids(&a, "idx").await, ["a", "b"]);
    let rows = a.scan("idx", None).await.unwrap();
    assert!(
        rows.iter()
            .all(|d| d.id != "b" || d.attrs["n"] == Value::Int(2))
    );
    let s = a.index_stats("idx").await.unwrap().unwrap();
    assert_eq!((s.rejected_rows, s.quarantined_rows), (4, 4), "{s:?}");
    let q = a.quarantine("idx").await.unwrap().unwrap();
    let mut kept: Vec<_> = q.rows.iter().map(|r| r.document.id.clone()).collect();
    kept.sort();
    assert_eq!(kept, ["a", "b", "c", "d"]);
    // Clearing the flag lets writes in again, and replays nothing it set aside.
    a.set_read_only("idx", false).await.unwrap().unwrap();
    b.write("idx", vec![doc("e", 5)]).await.unwrap();
    fold(&b).await;
    assert_eq!(ids(&a, "idx").await, ["a", "b", "e"]);
    assert_eq!(a.quarantine("idx").await.unwrap().unwrap().rows.len(), 4);
}

#[tokio::test]
async fn a_fold_losing_to_the_flag_sets_aside() {
    let (a, b) = engines();
    a.write("idx", vec![doc("a", 1)]).await.unwrap();
    a.write("z", vec![doc("z", 1)]).await.unwrap();
    fold(&a).await;
    b.write("idx", vec![doc("held", 2)]).await.unwrap();
    b.flush().await.unwrap();
    // A drop of `z` is a fold carrying `b`'s row; the flag commits between its read and its
    // CAS, so it loses and its retry reads the flag.
    let dropped = b
        .delete_index_with_interference_for_test("z", async {
            a.set_read_only("idx", true).await.unwrap().unwrap();
        })
        .await
        .unwrap();
    assert!(dropped.is_some());
    assert_eq!(ids(&a, "idx").await, ["a"], "a row folded past the flag");
    let s = a.index_stats("idx").await.unwrap().unwrap();
    assert_eq!(s.rejected_rows, 1);
    assert!(a.index_stats("z").await.unwrap().is_none());
}

#[tokio::test]
async fn read_only_lifecycle() {
    let (a, b) = engines();
    a.write("src", vec![doc("a", 1)]).await.unwrap();
    fold(&a).await;
    a.set_read_only("src", true).await.unwrap().unwrap();
    // A branch is a new index, and writable.
    a.branch("src", "copy").await.unwrap();
    assert!(!a.index_stats("copy").await.unwrap().unwrap().read_only);
    b.write("copy", vec![doc("b", 2)]).await.unwrap();
    fold(&b).await;
    assert_eq!(ids(&a, "copy").await, ["a", "b"]);
    // Dropped once cleared; created again, writable.
    read_only(a.delete_index("src").await, "src");
    a.set_read_only("src", false).await.unwrap().unwrap();
    a.delete_index("src").await.unwrap().unwrap();
    assert!(!a.head_for_test().await.read_only.contains("src"));
    a.create_index("src", 2, Metric::DotProduct).await.unwrap();
    assert!(!a.index_stats("src").await.unwrap().unwrap().read_only);
    b.write("src", vec![doc("c", 3)]).await.unwrap();
    fold(&b).await;
    assert_eq!(ids(&a, "src").await, ["c"]);
}
