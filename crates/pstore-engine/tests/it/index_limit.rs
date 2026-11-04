//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M169: a tenant's index count is capped where a name enters HEAD, and never at the fold.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass, TenantView};
use pstore_engine::{Engine, EngineError, ExternalKind, Metric, ReplicaSource};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(169);
const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");

type Store = TenantView<MemoryStore>;

fn engine(acc: &Accounted<MemoryStore>, lane: u64, limit: usize) -> Engine<Store> {
    Engine::new(Arc::new(acc.as_tenant(T)), T, LaneId(lane)).with_index_limit(limit)
}

fn rows(index: &str) -> Vec<Document> {
    vec![Document::new(format!("{index}-1"), vec![1.0, 0.5])]
}

async fn folded(e: &Engine<Store>, names: &[&str]) {
    for n in names {
        e.write(n, rows(n)).await.unwrap();
    }
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

fn is_limit(r: Result<impl std::fmt::Debug, EngineError>, limit: usize, what: &str) {
    match r {
        Err(EngineError::IndexLimit { limit: l }) => assert_eq!(l, limit, "{what}"),
        other => panic!("{what}: {other:?}"),
    }
}

#[tokio::test]
async fn a_create_past_the_limit_is_refused() {
    let acc = Accounted::new(MemoryStore::new());
    let e = engine(&acc, 1, 3);
    assert_eq!(e.index_limit(), 3);
    for n in ["a", "b"] {
        e.create_index(n, 2, Metric::DotProduct).await.unwrap();
    }
    // The first attempt reads two indexes; another create lands before its commit, so the
    // retry reads three and must refuse.
    let other = engine(&acc, 2, 0);
    is_limit(
        e.create_index_with_interference_for_test("d", 2, Metric::DotProduct, async {
            other
                .create_index("c", 2, Metric::DotProduct)
                .await
                .unwrap();
        })
        .await,
        3,
        "a create that lost its CAS to the limit",
    );
    let epoch = e.indexes_at().await.unwrap().0;
    is_limit(
        e.create_index("d", 2, Metric::DotProduct).await,
        3,
        "a create at the limit",
    );
    assert_eq!(
        e.indexes_at().await.unwrap(),
        (epoch, vec!["a".into(), "b".into(), "c".into()])
    );
    e.delete_index("a").await.unwrap();
    e.create_index("d", 2, Metric::DotProduct).await.unwrap();
    // Unbounded is unbounded.
    let free = engine(&acc, 3, 0);
    free.create_index("e", 2, Metric::DotProduct).await.unwrap();
}

#[tokio::test]
async fn branch_replication_and_external_respect_the_limit() {
    let acc = Accounted::new(MemoryStore::new());
    let raw = std::fs::read(format!("{DATA}good.parquet")).unwrap();
    acc.as_tenant(T)
        .put(&Key::new("lake/good.parquet"), raw.into())
        .await
        .unwrap();
    let lake = || vec!["lake/".to_owned()];
    let e = engine(&acc, 1, 2).with_external_sources(lake());
    folded(&e, &["a"]).await;
    e.register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap();
    let src = |index: &str| ReplicaSource {
        store: String::new(),
        tenant: T,
        index: index.to_owned(),
    };
    let from = acc.as_tenant(T);
    let writes = acc.total(OpClass::Write);
    is_limit(e.branch("a", "b").await, 2, "branch");
    is_limit(
        e.create_replication("r", src("a"), &from).await,
        2,
        "replication",
    );
    is_limit(e.create_copy("c", src("a"), &from).await, 2, "copy");
    is_limit(
        e.register_external("y", ExternalKind::Parquet, "lake/good.parquet", "embedding")
            .await,
        2,
        "external",
    );
    is_limit(
        e.copy_external("z", src("x"), &from).await,
        2,
        "external copy",
    );
    assert_eq!(acc.total(OpClass::Write), writes, "a refusal wrote");
    // One more of room admits each.
    let roomy = engine(&acc, 2, 6).with_external_sources(lake());
    roomy.branch("a", "b").await.unwrap();
    roomy
        .create_replication("r", src("a"), &from)
        .await
        .unwrap();
    roomy.create_copy("c", src("a"), &from).await.unwrap();
    roomy
        .register_external("y", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap();
    is_limit(
        roomy.copy_external("z", src("x"), &from).await,
        6,
        "external copy at six",
    );
}

#[tokio::test]
async fn a_write_past_the_limit_is_refused() {
    let acc = Accounted::new(MemoryStore::new());
    let e = engine(&acc, 1, 3);
    folded(&e, &["a", "b"]).await;
    // Buffered, unfolded: counted by this process.
    e.write("c", rows("c")).await.unwrap();
    is_limit(
        e.write("d", rows("d")).await,
        3,
        "a fourth name, one unfolded",
    );
    e.write("a", rows("a")).await.unwrap();
    e.write("c", rows("c")).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    is_limit(
        e.write("d", rows("d")).await,
        3,
        "a fourth name, all folded",
    );
    e.write("b", rows("b")).await.unwrap();
}

#[tokio::test]
async fn a_stale_door_rereads_before_refusing() {
    let acc = Accounted::new(MemoryStore::new());
    let e = engine(&acc, 1, 2);
    folded(&e, &["a", "b"]).await;
    // Another process drops `a`; this one's door still counts it.
    engine(&acc, 2, 2).delete_index("a").await.unwrap();
    let reads = acc.total(OpClass::Read);
    e.write("c", rows("c")).await.unwrap();
    assert_eq!(
        acc.total(OpClass::Read) - reads,
        1,
        "admitted after one HEAD read"
    );
    let reads = acc.total(OpClass::Read);
    is_limit(e.write("d", rows("d")).await, 2, "b and an unfolded c");
    assert_eq!(
        acc.total(OpClass::Read) - reads,
        1,
        "refused after one HEAD read"
    );
    let reads = acc.total(OpClass::Read);
    e.write("b", rows("b")).await.unwrap();
    assert_eq!(
        acc.total(OpClass::Read),
        reads,
        "a known name reads nothing"
    );
}

#[tokio::test]
async fn the_fold_never_refuses_an_admitted_write() {
    let acc = Accounted::new(MemoryStore::new());
    let one = engine(&acc, 1, 2);
    folded(&one, &["a"]).await;
    let two = engine(&acc, 2, 2);
    two.write("a", rows("a")).await.unwrap();
    // Each process counts one index, and admits one new name: together, three.
    one.write("b", rows("b")).await.unwrap();
    two.write("c", rows("c")).await.unwrap();
    one.flush().await.unwrap();
    two.flush().await.unwrap();
    one.fold().await.unwrap();
    two.fold().await.unwrap();
    let (_, names) = one.indexes_at().await.unwrap();
    assert_eq!(names, vec!["a", "b", "c"], "both admitted writes folded");
    let three = engine(&acc, 3, 2);
    is_limit(
        three.write("d", rows("d")).await,
        2,
        "a new process past the limit",
    );
    is_limit(
        one.write("d", rows("d")).await,
        2,
        "a writer past the limit",
    );
}
