//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Branching an index (M16): shared segments, one delete-vector key per (index, segment),
//! graveyard markers, and GC that reaps a doubly buried key only once both burials are past.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::{Document, Value};
use pstore_query::OrderBy;
use pstore_types::{Epoch, LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;

const T: TenantId = TenantId(70);

fn engine(store: &Arc<MemoryStore>) -> Engine<MemoryStore> {
    Engine::new(Arc::clone(store), T, LaneId(1))
}

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

/// Every row of `index` by id, with `n`, as of `at` -- or `None` when it does not exist.
async fn rows(
    e: &Engine<MemoryStore>,
    index: &str,
    at: Option<Epoch>,
) -> Option<BTreeMap<String, Value>> {
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let o = e.ordered(index, &by, None, 0, 100_000, at).await.unwrap();
    o.exists.then(|| {
        o.rows
            .into_iter()
            .map(|d| (d.id, d.attrs.get("n").cloned().unwrap()))
            .collect()
    })
}

async fn fold(e: &Engine<MemoryStore>) {
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

/// 300 rows in three folds, then a delete of a row in each segment.
async fn seeded(e: &Engine<MemoryStore>, index: &str) {
    for k in 0..3 {
        let docs = (k * 100..(k + 1) * 100)
            .map(|i| doc(&format!("r{i:04}"), i))
            .collect();
        e.write(index, docs).await.unwrap();
        fold(e).await;
    }
    e.delete(index, vec!["r0005".into(), "r0105".into(), "r0205".into()])
        .await
        .unwrap();
    fold(e).await;
}

fn keys(e: &pstore_engine::Head, index: &str) -> Vec<String> {
    let mut out: Vec<String> = e.indexes[index].iter().map(|r| r.key.clone()).collect();
    out.extend(e.deletes.values().map(|(k, _)| k.clone()));
    out
}

#[tokio::test]
async fn a_branch_equals_its_source_and_then_diverges() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store);
    seeded(&e, "src").await;
    e.branch("src", "dest").await.unwrap();
    let at_birth = rows(&e, "src", None).await.unwrap();
    assert_eq!(at_birth.len(), 297);
    assert_eq!(rows(&e, "dest", None).await.unwrap(), at_birth);
    // Independent after, both ways, in shared segments.
    e.delete("dest", vec!["r0010".into()]).await.unwrap();
    e.delete("src", vec!["r0110".into()]).await.unwrap();
    e.write("dest", vec![doc("new", -1)]).await.unwrap();
    fold(&e).await;
    let (src, dest) = (
        rows(&e, "src", None).await.unwrap(),
        rows(&e, "dest", None).await.unwrap(),
    );
    assert!(src.contains_key("r0010") && !dest.contains_key("r0010"));
    assert!(!src.contains_key("r0110") && dest.contains_key("r0110"));
    assert!(dest.contains_key("new") && !src.contains_key("new"));
    // A scan reads each index's own vectors too.
    for (index, want) in [("src", &src), ("dest", &dest)] {
        let scanned: Vec<String> = e
            .scan(index, None)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(scanned.len(), want.len(), "{index}");
        assert!(scanned.iter().all(|id| want.contains_key(id)), "{index}");
    }
    // Each compacts under its own vectors -- a merge that read the other's would drop the
    // wrong rows, or abandon itself as though a fold had changed them.
    for (index, want) in [("dest", &dest), ("src", &src)] {
        assert!(e.compact(index).await.unwrap().is_some(), "{index}");
        assert_eq!(&rows(&e, index, None).await.unwrap(), want, "{index}");
    }
    // `dest` dropped, then GC: `src` answers as before.
    e.delete_index("dest").await.unwrap();
    e.gc(0).await.unwrap();
    assert_eq!(rows(&e, "src", None).await.unwrap(), src);
}

#[tokio::test]
async fn gc_never_reaps_what_a_branch_names() {
    for drop_src in [false, true] {
        let store = Arc::new(MemoryStore::new());
        let e = engine(&store);
        seeded(&e, "src").await;
        e.branch("src", "dest").await.unwrap();
        let before = rows(&e, "dest", None).await.unwrap();
        if drop_src {
            e.delete_index("src").await.unwrap();
        } else {
            e.compact("src").await.unwrap();
        }
        e.gc(0).await.unwrap();
        assert_eq!(
            rows(&e, "dest", None).await.unwrap(),
            before,
            "drop_src {drop_src}"
        );
        // Then `dest` goes, and so does everything only it named.
        let named = keys(&e.head_for_test().await, "dest");
        e.delete_index("dest").await.unwrap();
        e.gc(0).await.unwrap();
        for k in named
            .iter()
            .filter(|k| !k.contains("/idx/src/") || drop_src)
        {
            assert!(
                store.get(&Key::new(k.clone())).await.is_err(),
                "{k} survived"
            );
        }
    }
}

#[tokio::test]
async fn a_doubly_buried_segment_waits_for_its_last_burial() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store);
    seeded(&e, "src").await;
    e.branch("src", "dest").await.unwrap();
    let shared = e.head_for_test().await.indexes["src"][0].key.clone();
    // `src` buries it at E2, `dest` at E4.
    let e2 = e.compact("src").await.unwrap().unwrap();
    let e4 = e.delete_index("dest").await.unwrap().unwrap();
    assert!(e2 < e4);
    // A horizon in [E2, E4): the E2 burial is due, the E4 one is not.
    e.gc(e4.0 - e2.0).await.unwrap();
    assert!(
        store.get(&Key::new(shared.clone())).await.is_ok(),
        "reaped inside the window"
    );
    e.gc(0).await.unwrap();
    assert!(store.get(&Key::new(shared)).await.is_err());
}

#[tokio::test]
async fn branches_nest_and_a_dropped_name_restores_from_its_branch() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store);
    seeded(&e, "a").await;
    let mut model = rows(&e, "a", None).await.unwrap();
    let mut models = Vec::new();
    for (from, to, gone) in [
        ("a", "b", "r0020"),
        ("b", "c", "r0120"),
        ("c", "d", "r0220"),
    ] {
        e.branch(from, to).await.unwrap();
        e.delete(to, vec![gone.into()]).await.unwrap();
        fold(&e).await;
        model.remove(gone);
        models.push((to, model.clone()));
    }
    for (index, want) in &models {
        assert_eq!(&rows(&e, index, None).await.unwrap(), want, "{index}");
    }
    // Restore: `a` dropped, its drop past GC's window, then branched back from `b`.
    let b = rows(&e, "b", None).await.unwrap();
    e.delete_index("a").await.unwrap();
    e.gc(0).await.unwrap();
    e.branch("b", "a").await.unwrap();
    assert_eq!(rows(&e, "a", None).await.unwrap(), b);
    e.delete("a", vec!["r0030".into()]).await.unwrap();
    fold(&e).await;
    assert!(rows(&e, "b", None).await.unwrap().contains_key("r0030"));
    assert!(!rows(&e, "a", None).await.unwrap().contains_key("r0030"));
}

#[tokio::test]
async fn a_branchs_past_is_its_own_and_begins_at_the_branch() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store);
    seeded(&e, "src").await;
    let before = e.head_for_test().await.epoch;
    e.branch("src", "dest").await.unwrap();
    e.delete("dest", vec!["r0040".into()]).await.unwrap();
    e.delete("src", vec!["r0140".into()]).await.unwrap();
    fold(&e).await;
    let then = e.head_for_test().await.epoch;
    let (src, dest) = (
        rows(&e, "src", None).await.unwrap(),
        rows(&e, "dest", None).await.unwrap(),
    );
    for step in 0..3 {
        match step {
            1 => {
                e.compact("dest").await.unwrap();
                e.compact("src").await.unwrap();
            }
            2 => {
                e.delete_index("dest").await.unwrap();
            }
            _ => {}
        }
        assert_eq!(
            rows(&e, "src", Some(then)).await.unwrap(),
            src,
            "step {step}"
        );
        assert_eq!(
            rows(&e, "dest", Some(then)).await.unwrap(),
            dest,
            "step {step}"
        );
    }
    assert_eq!(
        rows(&e, "dest", Some(before)).await,
        None,
        "the branch had no past"
    );
    assert!(rows(&e, "src", Some(before)).await.is_some());
}

#[tokio::test]
async fn a_branch_costs_a_read_and_a_cas_and_two_requests_per_vector() {
    let store = Arc::new(Accounted::new(MemoryStore::new()));
    let e = Engine::new(Arc::new(store.as_tenant(T)), T, LaneId(1));
    for k in 0..3 {
        let docs = (k * 100..(k + 1) * 100)
            .map(|i| doc(&format!("r{i:04}"), i))
            .collect();
        e.write("src", docs).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    // Deletes in two of the three segments: d = 2.
    e.delete("src", vec!["r0005".into(), "r0105".into()])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let (r, w) = (
        store.count(T, OpClass::Read),
        store.count(T, OpClass::Write),
    );
    e.branch("src", "dest").await.unwrap();
    assert_eq!(
        store.count(T, OpClass::Read) - r,
        1 + 2,
        "HEAD, then a GET per vector"
    );
    assert_eq!(
        store.count(T, OpClass::Write) - w,
        2 + 1,
        "a PUT per vector, then the CAS"
    );
    assert_eq!(store.count(T, OpClass::List), 0);
}

#[tokio::test]
async fn a_branch_that_loses_its_cas_buries_its_copies() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store);
    seeded(&e, "src").await;
    let other = engine(&store);
    // Another commit lands between the copies and the CAS: the first attempt loses.
    e.branch_with_interference_for_test("src", "dest", async {
        other.write("x", vec![doc("x", 0)]).await.unwrap();
        other.flush().await.unwrap();
        other.fold().await.unwrap();
    })
    .await
    .unwrap();
    let head = e.head_for_test().await;
    let named: Vec<&String> = head.deletes.values().map(|(k, _)| k).collect();
    let buried: Vec<&String> = head.graveyard.values().flatten().collect();
    // Every scoped copy is named by HEAD or buried: none leaks.
    for (seg, (k, _)) in &head.deletes {
        if seg.contains(".br-") {
            assert!(named.contains(&k));
        }
    }
    let losers = buried
        .iter()
        .filter(|k| k.contains(".br-") && k.ends_with(".dv"))
        .count();
    assert_eq!(
        losers, 3,
        "the lost attempt's three copies are buried: {buried:?}"
    );
    assert_eq!(rows(&e, "dest", None).await, rows(&e, "src", None).await);
}

#[tokio::test]
async fn an_index_whose_name_holds_seg_owns_its_segments() {
    // Only a branch's names are checked, so an index may be named `a/seg/b`: its segments'
    // keys hold `/seg/` twice, and the owner is read up to the last one -- or it would borrow
    // its own segments, and every delete vector written before M16 would be lost to it.
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store);
    let name = "a/seg/b";
    seeded(&e, name).await;
    let head = e.head_for_test().await;
    let segs: Vec<&String> = head.indexes[name].iter().map(|r| &r.key).collect();
    assert_eq!(head.deletes.len(), 3);
    assert!(
        head.deletes.keys().all(|k| segs.contains(&k)),
        "{:?}",
        head.deletes.keys()
    );
    // And compaction buries the segments themselves, which GC then reaps.
    e.compact(name).await.unwrap().unwrap();
    e.gc(0).await.unwrap();
    for k in segs {
        assert!(store.get(&Key::new(k.clone())).await.is_err(), "{k} kept");
    }
}
