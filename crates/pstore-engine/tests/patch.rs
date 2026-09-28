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
