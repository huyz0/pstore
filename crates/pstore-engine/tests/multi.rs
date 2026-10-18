//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M61: a named field of several vectors a document holds one width an index. No schema
//! records it, so two processes' rows -- each past its own door -- first meet in the fold.

use pstore_blob::{Accounted, BlobStore, MemoryStore};
use pstore_engine::Engine;
use pstore_format::{Document, VectorField};
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

const T: TenantId = TenantId(6100);

fn doc(id: &str, width: usize) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5, 0.25, 0.125]);
    d.vectors.insert(
        "late".to_owned(),
        VectorField::Dense(vec![vec![0.5; width], vec![-0.25; width]]),
    );
    d
}

#[tokio::test]
async fn two_writers_widths_meet_in_the_fold() {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let first = Engine::new(Arc::clone(&store), T, LaneId(1));
    let second = Engine::new(Arc::clone(&store), T, LaneId(2));
    first
        .write("docs", vec![doc("a", 3), doc("c", 3)])
        .await
        .unwrap();
    // Its own door holds no row of `late`: accepted.
    second.write("docs", vec![doc("b", 2)]).await.unwrap();
    first.flush().await.unwrap();
    second.flush().await.unwrap();
    first.fold().await.unwrap();
    // The first lane's rows decide the width; the second's row is set aside, intact.
    let q = first.quarantine("docs").await.unwrap().unwrap();
    let out: BTreeSet<String> = q.rows.iter().map(|r| r.document.id.clone()).collect();
    assert_eq!(out, BTreeSet::from(["b".to_owned()]));
    assert_eq!(
        q.rows[0].document.field("late"),
        doc("b", 2).field("late"),
        "quarantined intact"
    );
    // M67: the encoding the engine derived is not part of the row a client could write again.
    assert_eq!(
        q.rows[0].document.vectors,
        doc("b", 2).vectors,
        "no derived field in the export"
    );
    let multi = Prefetch::Multi {
        field: "late".to_owned(),
        query: vec![vec![1.0, 0.0, 0.0]],
        limit: 10,
    };
    let answer = first
        .query("docs", &[multi], Fusion::default(), 10)
        .await
        .unwrap();
    let ids: BTreeSet<String> = answer.ids.into_iter().flatten().collect();
    assert_eq!(ids, BTreeSet::from(["a".to_owned(), "c".to_owned()]));
}

#[tokio::test]
async fn a_patch_beside_a_new_width_never_stops_the_fold() {
    // Code review: a patch merges into its base row AFTER the fold's width pass, and the base
    // row keeps its folded `late`. With every row folded a new width passes the door, so one
    // fold could seal two widths, which the writer refuses -- and a fold is all-or-nothing
    // over bundles it re-reads, so every later fold of the tenant would fail with it.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 3)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // M98: the schema records `late`'s width, so the door, the flush and the reject pass all
    // refuse `x`. The second pass is reached by an index folded before that, which records
    // none: a process started on it accepts `x` through its own door.
    e.commit_head_for_test(|h| {
        for s in h.schemas.values_mut() {
            s.fields.clear();
        }
    })
    .await
    .unwrap();
    let other = Engine::new(Arc::clone(&store), T, LaneId(2));
    other.write("docs", vec![doc("x", 4)]).await.unwrap();
    other.flush().await.unwrap();
    let mut set = std::collections::BTreeMap::new();
    set.insert("tag".to_owned(), pstore_format::Value::Int(7));
    e.patch(
        "docs",
        vec![pstore_engine::Patch {
            id: "a".to_owned(),
            set,
            unset: Vec::new(),
        }],
        None,
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // The merged row that disagrees is set aside; the version already folded stands.
    let q = e.quarantine("docs").await.unwrap().unwrap();
    let out: BTreeSet<String> = q.rows.iter().map(|r| r.document.id.clone()).collect();
    assert_eq!(out, BTreeSet::from(["a".to_owned()]));
    assert_eq!(
        e.head_for_test().await.schema_rejects.get("docs").copied(),
        Some(1),
        "counted"
    );
    let rows = e.scan("docs", None).await.unwrap();
    let ids: BTreeSet<String> = rows.iter().map(|d| d.id.clone()).collect();
    assert_eq!(ids, BTreeSet::from(["a".to_owned(), "x".to_owned()]));
    let a = rows.iter().find(|d| d.id == "a").unwrap();
    assert!(
        !a.attrs.contains_key("tag"),
        "the patch was set aside: {:?}",
        a.attrs
    );
    // And the tenant still folds.
    e.write("docs", vec![doc("y", 4)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

#[tokio::test]
async fn a_named_field_holding_no_vectors_derives_no_encoding() {
    // M67: the engine's write encodes a named field's vectors; with none there is nothing to
    // encode, and the row still writes, folds and is served as written.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    let mut empty = Document::new("e", vec![1.0, 0.5, 0.25, 0.125]);
    empty
        .vectors
        .insert("late".to_owned(), VectorField::Dense(Vec::new()));
    e.write("docs", vec![doc("a", 3), empty]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let fde = Prefetch::MultiFde {
        field: "late".to_owned(),
        query: vec![vec![1.0, 0.0, 0.0]],
        limit: 10,
        rerank: None,
        probes: None,
    };
    let answer = e
        .query("docs", &[fde], Fusion::default(), 10)
        .await
        .unwrap();
    let ids: BTreeSet<String> = answer.ids.into_iter().flatten().collect();
    assert_eq!(ids, BTreeSet::from(["a".to_owned()]));
    let rows = e.scan("docs", None).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|d| d.vectors.keys().all(|k| !k.ends_with("$fde")))
    );
}

/// Documents of two vectors near one of twelve topics, so a clustering has lists to find.
fn topical(i: usize) -> Document {
    let topic = i % 12;
    let mut s = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut r = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 2000) as f32 / 1000.0 - 1.0
    };
    let base = |t: usize, d: usize| ((t * 7 + d * 3) % 11) as f32 - 5.0;
    let v = |r: &mut dyn FnMut() -> f32| -> Vec<f32> {
        (0..4).map(|d| base(topic, d) + 0.3 * r()).collect()
    };
    let mut doc = Document::new(format!("d{i:05}"), vec![1.0, 0.5, 0.25, 0.125]);
    doc.vectors.insert(
        "late".to_owned(),
        VectorField::Dense(vec![v(&mut r), v(&mut r)]),
    );
    doc
}

/// M71.2: a fold big enough to cluster writes the sidecars, HEAD records them, a query uses
/// them and finds what the flat scan finds, and compaction and GC keep them in step.
#[tokio::test]
async fn a_big_segment_clusters_its_encoding_and_a_query_uses_it() {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    let docs: Vec<Document> = (0..9_000).map(topical).collect();
    e.write("docs", docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let head = e.head_for_test().await;
    let refs: Vec<_> = head.indexes.get("docs").unwrap().clone();
    assert_eq!(refs.len(), 1);
    assert!(refs[0].dicts.unwrap().fde, "HEAD records the clusters");
    let seg = pstore_blob::Key::new(refs[0].key.clone());
    let sidecar = pstore_index::fde_clusters::FdeSet::open(&*store, &seg)
        .await
        .unwrap();
    let (clusters, _) = sidecar.field("late").unwrap();
    assert!(clusters.lists() > 1);

    let q = vec![vec![-2.0, 1.0, 4.0, 0.0], vec![3.0, -1.0, 0.0, 2.0]];
    let fde = Prefetch::MultiFde {
        field: "late".to_owned(),
        query: q.clone(),
        limit: 10,
        rerank: Some(100),
        probes: None,
    };
    let exact = Prefetch::Multi {
        field: "late".to_owned(),
        query: q,
        limit: 10,
    };
    let want = e
        .query("docs", &[exact], Fusion::default(), 10)
        .await
        .unwrap();
    let got = e
        .query("docs", std::slice::from_ref(&fde), Fusion::default(), 10)
        .await
        .unwrap();
    // ~750 documents a topic are near-ties, so the ids of the top 10 are arbitrary among them;
    // the scores are not. The clustered, reranked answer scores as the exact one does.
    let mean = |a: &pstore_engine::Answer| {
        a.hits.iter().map(|h| h.score).sum::<f32>() / a.hits.len() as f32
    };
    assert_eq!(got.hits.len(), 10);
    assert!(
        mean(&got) >= 0.95 * mean(&want),
        "{} vs {}",
        mean(&got),
        mean(&want)
    );

    // Without the sidecar the same query takes the flat scan and answers the same.
    store
        .delete_batch(&[pstore_index::fde_clusters::vectors_key(&seg)])
        .await
        .unwrap();
    assert!(
        e.query("docs", std::slice::from_ref(&fde), Fusion::default(), 10)
            .await
            .is_err(),
        "HEAD says it exists: a missing vectors blob is an error, not a silent flat scan"
    );

    // Compaction reseals it with its own sidecars (the vectors blob restored first, so the
    // inputs can be read), and GC reaps the old segment's.
    store
        .put(
            &pstore_index::fde_clusters::vectors_key(&seg),
            bytes::Bytes::new(),
        )
        .await
        .unwrap();
    e.write("docs", (9_000..9_050).map(topical).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert!(e.compact("docs").await.unwrap().is_some());
    let after = e.head_for_test().await;
    let merged = after.indexes.get("docs").unwrap();
    assert_eq!(merged.len(), 1, "compaction merged the segments");
    assert!(
        merged[0].dicts.unwrap().fde,
        "the merged segment has clusters"
    );
    assert_ne!(merged[0].key, refs[0].key);
    let fresh = pstore_blob::Key::new(merged[0].key.clone());
    assert!(
        store
            .get(&pstore_index::fde_clusters::key(&fresh))
            .await
            .is_ok()
    );
    e.gc(0).await.unwrap();
    assert!(
        store
            .get(&pstore_index::fde_clusters::key(&seg))
            .await
            .is_err(),
        "GC reaps the buried segment's clusters"
    );
    assert!(
        store
            .get(&pstore_index::fde_clusters::key(&fresh))
            .await
            .is_ok()
    );
}

/// `doc`, with a second named field `other` of one vector at `width`.
fn with_other(id: &str, late: usize, width: usize) -> Document {
    let mut d = doc(id, late);
    d.vectors.insert(
        "other".to_owned(),
        VectorField::Dense(vec![vec![0.75; width]]),
    );
    d
}

#[tokio::test]
async fn the_fold_records_each_named_fields_width() {
    // M98: in the schema, stored widths as `dims` is: euclidean adds a component.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    let euclid = pstore_engine::Metric::EuclideanSquared;
    e.write_as("docs", vec![doc("a", 3)], euclid).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let fields = |h: &pstore_engine::Head| h.schemas["docs"].fields.clone();
    let h = e.head_for_test().await;
    assert_eq!(
        fields(&h),
        std::collections::BTreeMap::from([("late".to_owned(), 4)])
    );
    // A later fold adds a field it first meets and leaves the one recorded.
    e.write_as("docs", vec![with_other("b", 3, 2)], euclid)
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let h = e.head_for_test().await;
    assert_eq!(
        fields(&h),
        std::collections::BTreeMap::from([("late".to_owned(), 4), ("other".to_owned(), 3)])
    );
    // And the door refuses another width of either, with nothing held: the schema says.
    for d in [doc("c", 2), with_other("c", 3, 5)] {
        let err = e.write_as("docs", vec![d], euclid).await.unwrap_err();
        assert!(err.to_string().contains("named vector field"), "{err}");
    }
}

#[tokio::test]
async fn a_held_width_meets_the_recorded_one_in_the_fold() {
    // M98: a row of another width past both checks -- the hooks build what two processes that
    // never read HEAD could -- is judged by the schema, not by the fold's first row of it.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 3)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // First in the fold, so a fold-local first row would have decided by it.
    e.write_without_schema_check_for_test("docs", vec![doc("x", 4)])
        .await;
    e.write_without_schema_check_for_test("docs", vec![doc("y", 3)])
        .await;
    e.flush_without_schema_check_for_test().await.unwrap();
    e.fold().await.unwrap();
    let q = e.quarantine("docs").await.unwrap().unwrap();
    let out: Vec<(String, Option<String>)> = q
        .rows
        .iter()
        .map(|r| (r.document.id.clone(), r.reason.clone()))
        .collect();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, "x");
    assert!(
        out[0]
            .1
            .as_deref()
            .is_some_and(|r| r.contains("named vector field")),
        "a reason: {out:?}"
    );
    // One width sealed, so the index compacts.
    e.compact("docs").await.unwrap();
    let rows = e.scan("docs", None).await.unwrap();
    let ids: BTreeSet<String> = rows.iter().map(|d| d.id.clone()).collect();
    assert_eq!(ids, BTreeSet::from(["a".to_owned(), "y".to_owned()]));
}

#[tokio::test]
async fn a_fold_without_a_field_keeps_its_width() {
    // M98 review: a fold whose rows carry no `late` records what it judged by, which is the
    // schema's widths and its own: never fewer.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 3)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("docs", vec![Document::new("b", vec![0.5; 4])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let h = e.head_for_test().await;
    assert_eq!(
        h.schemas["docs"].fields,
        std::collections::BTreeMap::from([("late".to_owned(), 3)])
    );
}

#[tokio::test]
async fn the_schema_outranks_a_held_width_at_the_door() {
    // M98 review: a row this process buffered before the schema recorded `late` is one the
    // fold will set aside; it must not refuse a write at the recorded width (M35).
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let a = Engine::new(Arc::clone(&store), T, LaneId(1));
    let b = Engine::new(Arc::clone(&store), T, LaneId(2));
    a.write("docs", vec![doc("p", 5)]).await.unwrap();
    b.write("docs", vec![doc("x", 3)]).await.unwrap();
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    a.write("docs", vec![doc("q", 3)]).await.unwrap();
    let err = a.write("docs", vec![doc("r", 5)]).await.unwrap_err();
    assert!(err.to_string().contains("named vector field"), "{err}");
}

#[tokio::test]
async fn a_patch_meets_the_recorded_width() {
    // M98: an index folded with two widths before M98 holds a row the schema's width now
    // contradicts. A patch resolves it after the reject pass, so only the second pass, seeded
    // with the schema's widths, sets it aside.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 3)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.commit_head_for_test(|h| {
        for s in h.schemas.values_mut() {
            s.fields.clear();
        }
    })
    .await
    .unwrap();
    let other = Engine::new(Arc::clone(&store), T, LaneId(2));
    other.write("docs", vec![doc("x", 4)]).await.unwrap();
    other.flush().await.unwrap();
    other.fold().await.unwrap();
    assert_eq!(
        e.head_for_test().await.schemas["docs"].fields,
        std::collections::BTreeMap::from([("late".to_owned(), 4)])
    );
    let mut set = std::collections::BTreeMap::new();
    set.insert("tag".to_owned(), pstore_format::Value::Int(7));
    other
        .patch(
            "docs",
            vec![pstore_engine::Patch {
                id: "a".to_owned(),
                set,
                unset: Vec::new(),
            }],
            None,
        )
        .await
        .unwrap();
    other.flush().await.unwrap();
    other.fold().await.unwrap();
    let q = other.quarantine("docs").await.unwrap().unwrap();
    let out: Vec<String> = q.rows.iter().map(|r| r.document.id.clone()).collect();
    assert_eq!(out, ["a"]);
    let a = other
        .scan("docs", None)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.id == "a")
        .unwrap();
    assert!(!a.attrs.contains_key("tag"), "set aside: {:?}", a.attrs);
}
