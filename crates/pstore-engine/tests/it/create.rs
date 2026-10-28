//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M147: an index created empty, or emptied by deletes and a compaction, exists with no
//! segment; and two creates of one name have one winner.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::{
    Consistency, Engine, EngineError, ExternalKind, ExternalRef, Metric, ReplicaRefusal,
    ReplicaSource, Replication,
};
use pstore_format::Document;
use pstore_index::vec_index::Query;
use pstore_query::{Aggregate, AggregateSpec, Fusion, OrderBy, Prefetch};
use pstore_testkit::claims::Claims;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(147);

fn dense(q: Vec<f32>) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: q,
        limit: 10,
        tune: Query::default(),
    }]
}

/// Every read the engine answers for `index` says it exists and holds nothing.
async fn exists_and_is_empty(e: &Engine<MemoryStore>, index: &str, dims: usize) {
    let a = e
        .query(index, &dense(vec![1.0; dims]), Fusion::default(), 5)
        .await
        .unwrap();
    assert!(a.named, "relevance: the HEAD names {index}");
    assert!(a.segments.is_empty() && a.hits.is_empty());
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let o = e.ordered(index, &by, None, 0, 10, None).await.unwrap();
    assert!(o.exists && o.rows.is_empty(), "ordered: {o:?}");
    let spec = AggregateSpec {
        labels: vec![("n".to_owned(), Aggregate::Count(None))],
        group_by: vec![],
        top_k: 10,
    };
    let g = e
        .aggregate_as(index, spec, None, None, Consistency::Eventual)
        .await
        .unwrap();
    assert!(g.exists, "aggregate: {g:?}");
    let w = e.warm(index).await.unwrap();
    assert!(w.exists && w.segments == 0, "warm: {w:?}");
}

#[tokio::test]
async fn a_created_index_exists_on_every_read() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    let epoch = e
        .create_index("x", 2, Metric::CosineDistance)
        .await
        .unwrap();
    exists_and_is_empty(&e, "x", 2).await;
    let head = e.head_for_test().await;
    assert_eq!(head.epoch, epoch);
    assert_eq!(head.indexes["x"], vec![]);
    assert_eq!(head.schemas["x"].metric, Metric::CosineDistance);
    // The stored width: cosine adds no component.
    assert_eq!(head.schemas["x"].dims, 2);
    // Another process, cold.
    exists_and_is_empty(&Engine::new(store, T, LaneId(2)), "x", 2).await;
}

#[tokio::test]
async fn an_index_emptied_by_deletes_and_compaction_exists_on_every_read() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    for id in ["a", "b"] {
        e.write("x", vec![Document::new(id, vec![1.0, 0.5])])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.delete("x", vec!["a".into(), "b".into()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.compact("x").await.unwrap().unwrap();
    assert_eq!(
        e.head_for_test().await.indexes["x"],
        vec![],
        "the fixture must leave an empty list"
    );
    exists_and_is_empty(&e, "x", 2).await;
}

#[tokio::test]
async fn concurrent_creates_have_one_winner() {
    let store = Arc::new(MemoryStore::new());
    let (e, w) = (
        Engine::new(Arc::clone(&store), T, LaneId(1)),
        Engine::new(Arc::clone(&store), T, LaneId(2)),
    );
    let err = e
        .create_index_with_interference_for_test("x", 2, Metric::CosineDistance, async {
            w.create_index("x", 3, Metric::EuclideanSquared)
                .await
                .unwrap();
        })
        .await
        .expect_err("the losing create's retry must see the winner");
    assert!(
        matches!(err, EngineError::IndexExists(ref n) if n == "x"),
        "{err:?}"
    );
    let head = e.head_for_test().await;
    // The winner's: three components, and euclidean's extra one.
    assert_eq!(head.schemas["x"].metric, Metric::EuclideanSquared);
    assert_eq!(head.schemas["x"].dims, 4);
}

#[tokio::test]
async fn a_create_refuses_what_exists_and_writes_nothing() {
    let acc = Accounted::new(MemoryStore::new());
    let e = Engine::new(Arc::new(acc.as_tenant(T)), T, LaneId(1));
    e.write("held", vec![Document::new("a", vec![1.0])])
        .await
        .unwrap();
    let before = acc.total(OpClass::Write);
    let err = e
        .create_index("held", 1, Metric::DotProduct)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::IndexExists(_)), "{err:?}");
    for (name, dims) in [("ok", 0), ("..", 1), ("", 1)] {
        let err = e
            .create_index(name, dims, Metric::DotProduct)
            .await
            .unwrap_err();
        assert!(
            matches!(err, EngineError::Refused(_)),
            "{name:?} {dims}: {err:?}"
        );
    }
    let err = e
        .create_index("big", u32::MAX, Metric::EuclideanSquared)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Refused(_)), "{err:?}");
    assert_eq!(acc.total(OpClass::Write), before, "a refusal wrote");
    // The largest width that fits beside euclidean's component.
    e.create_index("big", u32::MAX - 1, Metric::EuclideanSquared)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_backend_that_cannot_fence_refuses_a_create() {
    let acc = Accounted::new(Claims::divergent_cas("wildcard ignored"));
    let e = Engine::new(Arc::new(acc.as_tenant(T)), T, LaneId(0));
    let err = e
        .create_index("x", 2, Metric::DotProduct)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::BackendCannotFence { .. }),
        "{err:?}"
    );
    assert_eq!(acc.total(OpClass::Write), 0);
    assert_eq!(acc.total(OpClass::Read), 0);
}

#[tokio::test]
async fn a_create_refuses_each_kind_of_name_head_holds() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    // Each kind alone in HEAD, as a fold or an operator would leave it.
    e.commit_head_for_test(|h| {
        h.schema_rejects.insert("rejects".to_owned(), 1);
        h.quarantine
            .insert("quarantined".to_owned(), vec![("q".to_owned(), 1)]);
        h.externals.insert(
            "external".to_owned(),
            ExternalRef {
                kind: ExternalKind::Parquet,
                source: "s".to_owned(),
                column: "c".to_owned(),
                codes: "k".to_owned(),
                rows: 1,
                source_len: 1,
            },
        );
        h.replications.insert(
            "replica".to_owned(),
            Replication {
                source: ReplicaSource::local(T, "src"),
                running: true,
                run: 1,
                applied: None,
                rejected: 0,
            },
        );
    })
    .await
    .unwrap();
    for name in ["rejects", "quarantined", "external"] {
        let err = e
            .create_index(name, 2, Metric::DotProduct)
            .await
            .unwrap_err();
        assert!(
            matches!(err, EngineError::IndexExists(_)),
            "{name}: {err:?}"
        );
    }
    let err = e
        .create_index("replica", 2, Metric::DotProduct)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Replica(ReplicaRefusal::ReadOnly(_))),
        "{err:?}"
    );
    assert!(!e.head_for_test().await.indexes.contains_key("replica"));
}

#[tokio::test]
async fn a_bounded_read_after_a_create_sees_it() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    let bounded = Consistency::Bounded(std::time::Duration::from_secs(3600));
    let ask = |index: &'static str| {
        let e = &e;
        async move {
            e.query_split_as(
                index,
                &dense(vec![1.0, 0.0]),
                None,
                Fusion::default(),
                5,
                bounded,
                None,
            )
            .await
            .unwrap()
        }
    };
    // Caches the HEAD from before the create.
    assert!(!ask("x").await.named);
    e.create_index("x", 2, Metric::DotProduct).await.unwrap();
    assert!(
        ask("x").await.named,
        "served the HEAD from before the create"
    );
}
