//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Deferred operations at the fold (M13.1), where the API cannot reach: a sparse vector kept
//! through a patch, and a conditional upsert judged by the fold's reject pass.

use pstore_blob::MemoryStore;
use pstore_engine::{Engine, Metric, Patch};
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_query::{Op, Predicate};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn a_patch_keeps_the_rows_sparse_and_dense_vectors() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(50), LaneId(1));
    let mut d = Document::new("x", vec![1.0, 0.5]);
    d.vectors.insert(
        "s".to_owned(),
        VectorField::Sparse(vec![(3, Impact::new(0.5)), (9, Impact::new(0.25))]),
    );
    d.attrs.insert("a".to_owned(), Value::Int(1));
    e.write("idx", vec![d.clone(), Document::new("y", vec![0.0, 1.0])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let patch = Patch {
        id: "x".to_owned(),
        set: BTreeMap::from([("a".to_owned(), Value::Int(2))]),
        unset: vec![],
    };
    e.patch("idx", vec![patch], None).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let segments = e.segment_rows_for_test("idx").await.unwrap();
    let newest = segments.last().unwrap();
    let x = newest.iter().find(|r| r.id == "x").expect("x resealed");
    assert_eq!(x.attrs.get("a"), Some(&Value::Int(2)));
    assert_eq!(x.vectors, d.vectors, "a vector changed through a patch");
}

#[tokio::test]
async fn a_conditional_upsert_is_judged_by_the_reject_pass() {
    // Two writers create one index at once, so neither door has a schema to check against:
    // the fold's reject pass is the only check. The conditional upsert is the index's first
    // row, so it sets the schema, and the other writer's wider row is the one rejected. Were
    // conditional upserts exempt, the wider row would set the schema and this one would be
    // sealed beside it at the wrong width.
    let store = Arc::new(MemoryStore::new());
    let (t, idx) = (TenantId(51), "idx");
    let one = Engine::new(Arc::clone(&store), t, LaneId(1));
    let two = Engine::new(Arc::clone(&store), t, LaneId(2));
    let always = Predicate::Not(Box::new(Predicate::Cmp(
        "never".to_owned(),
        Op::Eq,
        Value::Int(0),
    )));
    one.write_if(
        idx,
        vec![Document::new("a", vec![1.0, 0.5])],
        Metric::DotProduct,
        &always,
    )
    .await
    .unwrap();
    one.flush().await.unwrap();
    two.write(idx, vec![Document::new("b", vec![1.0, 0.5, 2.0])])
        .await
        .unwrap();
    two.flush().await.unwrap();
    one.fold().await.unwrap();
    let stats = one.index_stats(idx).await.unwrap().unwrap();
    assert_eq!(stats.rejected_rows, 1, "{stats:?}");
    assert_eq!(stats.documents, 1);
    assert_eq!(stats.schema.unwrap().client_dims(), 2);
}

#[tokio::test]
async fn a_patch_after_a_delete_supersedes_the_right_row_across_blocks() {
    // Positions come from an unfiltered scan in row order; a shift would supersede a
    // neighbour. 300 rows span several blocks; an earlier row is already deleted.
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(52), LaneId(1));
    let docs: Vec<Document> = (0..300)
        .map(|i| {
            let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.5]);
            d.attrs.insert("n".to_owned(), Value::Int(i));
            d.attrs.insert("m".to_owned(), Value::Int(2 * i));
            d
        })
        .collect();
    let vectors = docs[200].vectors.clone();
    e.write("idx", docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.delete("idx", vec!["d00010".to_owned()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let patch = Patch {
        id: "d00200".to_owned(),
        set: BTreeMap::from([("n".to_owned(), Value::Int(-1))]),
        unset: vec![],
    };
    e.patch("idx", vec![patch], None).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let mut rows: Vec<Document> = e.scan("idx", None).await.unwrap();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(rows.len(), 299);
    for r in &rows {
        let i: i64 = r.id[1..].parse().unwrap();
        let want = if i == 200 { -1 } else { i };
        assert_eq!(r.attrs.get("n"), Some(&Value::Int(want)), "{}", r.id);
        assert_eq!(r.attrs.get("m"), Some(&Value::Int(2 * i)), "{}", r.id);
    }
    // The patched row is its whole base version, vector included, with `n` set.
    let patched = rows.iter().find(|r| r.id == "d00200").unwrap();
    assert_eq!(patched.vectors, vectors);
}

#[tokio::test]
async fn a_patch_of_a_value_no_segment_stores_is_refused() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(53), LaneId(1));
    for bad in [
        Value::Float(f64::NAN),
        Value::Array(vec![Value::Array(vec![])]),
    ] {
        let patch = Patch {
            id: "x".to_owned(),
            set: BTreeMap::from([("a".to_owned(), bad.clone())]),
            unset: vec![],
        };
        let got = e.patch("idx", vec![patch], None).await;
        assert!(got.is_err(), "{bad:?} was accepted");
    }
    assert_eq!(
        e.flush().await.unwrap(),
        None,
        "a refused patch was buffered"
    );
}

#[tokio::test]
async fn a_condition_the_fold_cannot_read_is_refused_at_the_call() {
    // Deeper than the encoding carries: the fold would read it as admitting nothing, and
    // the operation would vanish after being acknowledged (code review round 2, M13.1).
    let mut deep = Predicate::Cmp("a".to_owned(), Op::Eq, Value::Int(1));
    for _ in 0..80 {
        deep = Predicate::Not(Box::new(deep));
    }
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(54), LaneId(1));
    let x = || Document::new("x", vec![1.0, 0.5]);
    let patch = Patch {
        id: "x".to_owned(),
        set: BTreeMap::from([("a".to_owned(), Value::Int(2))]),
        unset: vec![],
    };
    assert!(
        e.write_if("idx", vec![x()], Metric::DotProduct, &deep)
            .await
            .is_err()
    );
    assert!(e.patch("idx", vec![patch], Some(&deep)).await.is_err());
    assert!(
        e.delete_if("idx", vec!["x".to_owned()], &deep)
            .await
            .is_err()
    );
    assert_eq!(
        e.flush().await.unwrap(),
        None,
        "a refused operation was buffered"
    );
}

#[tokio::test]
async fn a_by_filter_operation_judges_whole_rows_not_the_ids_a_fold_keeps() {
    // The fold keeps whole rows only where an operation needs them, and ids alone for the
    // rest. `Absent` admits an id-only row, so a filter judged there would delete `x`.
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(55), LaneId(1));
    let row = |id: &str, k: &str| {
        let mut d = Document::new(id, vec![1.0, 0.5]);
        d.attrs.insert(k.to_owned(), Value::Int(1));
        d
    };
    e.write("idx", vec![row("x", "a"), row("y", "b")])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.delete_by_filter("idx", &Predicate::Absent("a".to_owned()))
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let ids: Vec<String> = e
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, ["x"]);
}

#[tokio::test]
async fn a_by_filter_filter_the_fold_cannot_read_is_refused_at_the_call() {
    let mut deep = Predicate::Cmp("a".to_owned(), Op::Eq, Value::Int(1));
    for _ in 0..80 {
        deep = Predicate::Not(Box::new(deep));
    }
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(56), LaneId(1));
    let patch = Patch {
        id: String::new(),
        set: BTreeMap::from([("a".to_owned(), Value::Int(2))]),
        unset: vec![],
    };
    assert!(e.delete_by_filter("idx", &deep).await.is_err());
    assert!(e.patch_by_filter("idx", &deep, patch).await.is_err());
    assert_eq!(
        e.flush().await.unwrap(),
        None,
        "a refused operation was buffered"
    );
}

#[tokio::test]
async fn a_patch_of_a_reserved_name_is_refused() {
    // The engine's own door, which the API's never lets a `$` name reach (M13 sweep).
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(57), LaneId(1));
    for name in ["$op", ""] {
        let set = Patch {
            id: "x".to_owned(),
            set: BTreeMap::from([(name.to_owned(), Value::Int(1))]),
            unset: vec![],
        };
        let unset = Patch {
            id: "x".to_owned(),
            set: BTreeMap::new(),
            unset: vec![name.to_owned()],
        };
        assert!(
            e.patch("idx", vec![set], None).await.is_err(),
            "set {name:?}"
        );
        assert!(
            e.patch("idx", vec![unset], None).await.is_err(),
            "unset {name:?}"
        );
    }
    assert_eq!(
        e.flush().await.unwrap(),
        None,
        "a refused patch was buffered"
    );
}

#[tokio::test]
async fn a_patch_never_resurrects_a_row_a_delete_vector_buries() {
    // `x` is deleted at one fold and patched -- by id, and by a filter its old row admits --
    // at the next. Its old row is still in its segment, buried only by the delete vector, so
    // a fold that read it as a current version would patch it back to life.
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(57), LaneId(1));
    let mut x = Document::new("x", vec![1.0, 0.5]);
    x.attrs.insert("a".to_owned(), Value::Int(1));
    e.write("idx", vec![x, Document::new("y", vec![0.0, 1.0])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.delete("idx", vec!["x".to_owned()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let set = || BTreeMap::from([("b".to_owned(), Value::Int(2))]);
    e.patch(
        "idx",
        vec![Patch {
            id: "x".to_owned(),
            set: set(),
            unset: vec![],
        }],
        None,
    )
    .await
    .unwrap();
    e.patch_by_filter(
        "idx",
        &Predicate::Cmp("a".to_owned(), Op::Eq, Value::Int(1)),
        Patch {
            id: String::new(),
            set: set(),
            unset: vec![],
        },
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let ids: Vec<String> = e
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, ["y"]);
}
