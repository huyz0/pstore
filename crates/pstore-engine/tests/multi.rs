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

use pstore_blob::{Accounted, MemoryStore};
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
    e.write("docs", vec![doc("x", 4)]).await.unwrap();
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
