//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M92: an external index copied to a new name, its codes moved and nothing rebuilt.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_engine::{Engine, EngineError, ExternalKind, ExternalRefusal, ReplicaSource};
use pstore_testkit::flaky::Flaky;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
const T: TenantId = TenantId(92);
const SOURCE: &str = "lake/good.parquet";

fn good() -> Vec<u8> {
    std::fs::read(format!("{DATA}good.parquet")).unwrap()
}

fn row(i: usize) -> Vec<f32> {
    std::fs::read(format!("{DATA}good.vectors.f32"))
        .unwrap()
        .chunks_exact(4)
        .skip(i * 8)
        .take(8)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

fn bits(hits: &[(usize, f32)]) -> Vec<(usize, u32)> {
    hits.iter().map(|(r, s)| (*r, s.to_bits())).collect()
}

/// An engine over `store` for `tenant` that may read under `lake/`, after the customer's file is
/// there.
async fn engine<S: BlobStore>(store: &Arc<S>, tenant: TenantId, lane: u64) -> Engine<S> {
    store.put(&Key::new(SOURCE), good().into()).await.unwrap();
    Engine::new(Arc::clone(store), tenant, LaneId(lane))
        .with_external_sources(vec!["lake/".to_owned()])
}

async fn registered<S: BlobStore>(e: &Engine<S>) {
    e.register_external("x", ExternalKind::Parquet, SOURCE, "embedding")
        .await
        .unwrap();
}

fn local(tenant: TenantId) -> ReplicaSource {
    ReplicaSource::local(tenant, "x")
}

#[tokio::test]
async fn a_copy_answers_as_its_source_does() {
    let acct = Accounted::new(MemoryStore::new());
    let store = Arc::new(acct.as_tenant(T));
    let e = engine(&store, T, 1).await;
    registered(&e).await;
    let want = e.query_external("x", &row(5), 10, 40).await.unwrap();

    // In this tenant, under a new name: the customer's object is not touched.
    acct.record_ranges();
    let epoch = e.copy_external("y", local(T), &*store).await.unwrap();
    assert!(epoch.0 > 0);
    assert!(
        acct.ranges().iter().all(|(k, _)| k.as_str() != SOURCE),
        "the copy read the customer's object: {:?}",
        acct.ranges()
    );
    let head = e.head_for_test().await;
    let (x, y) = (&head.externals["x"], &head.externals["y"]);
    assert_ne!(x.codes, y.codes, "a copy has codes of its own");
    assert_eq!(
        (x.kind, &x.source, &x.column, x.rows, x.source_len),
        (y.kind, &y.source, &y.column, y.rows, y.source_len)
    );
    let got = e.query_external("y", &row(5), 10, 40).await.unwrap();
    assert_eq!(bits(&got), bits(&want));

    // From another tenant of the same bucket.
    let other = TenantId(93);
    let theirs = Arc::new(acct.as_tenant(other));
    let b = Engine::new(Arc::clone(&theirs), other, LaneId(1))
        .with_external_sources(vec!["lake/".to_owned()]);
    b.copy_external("z", local(T), &*store).await.unwrap();
    let got = b.query_external("z", &row(5), 10, 40).await.unwrap();
    assert_eq!(bits(&got), bits(&want));
}

#[tokio::test]
async fn a_copy_from_another_store_reads_the_customers_object_in_its_own() {
    let there = Arc::new(MemoryStore::new());
    let a = engine(&there, T, 1).await;
    registered(&a).await;
    let want = a.query_external("x", &row(3), 10, 40).await.unwrap();
    let from = ReplicaSource {
        store: "eu".to_owned(),
        tenant: T,
        index: "x".to_owned(),
    };

    // The copying store holds the same object: it answers as the source does.
    let here = Arc::new(MemoryStore::new());
    let b = engine(&here, TenantId(94), 1).await;
    b.copy_external("y", from.clone(), &*there).await.unwrap();
    let got = b.query_external("y", &row(3), 10, 40).await.unwrap();
    assert_eq!(bits(&got), bits(&want));

    // Holding none, it is refused by name -- never answered.
    let empty = Arc::new(MemoryStore::new());
    let c = Engine::new(Arc::clone(&empty), TenantId(95), LaneId(1))
        .with_external_sources(vec!["lake/".to_owned()]);
    c.copy_external("y", from.clone(), &*there).await.unwrap();
    let err = c.query_external("y", &row(3), 10, 40).await.unwrap_err();
    assert!(
        matches!(&err, EngineError::External(ExternalRefusal::Source(m)) if m.contains("no object")),
        "{err}"
    );

    // Holding an object that is not the pinned one, it is `Changed`.
    let other = Arc::new(MemoryStore::new());
    let d = engine(&other, TenantId(96), 1).await;
    let mut raw = good();
    raw[4] ^= 0xFF;
    other.put(&Key::new(SOURCE), raw.into()).await.unwrap();
    d.copy_external("y", from, &*there).await.unwrap();
    let err = d.query_external("y", &row(3), 10, 40).await.unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Changed)),
        "{err}"
    );
}

#[tokio::test]
async fn a_copy_is_refused_unless_this_deployment_allows_it() {
    let acct = Accounted::new(MemoryStore::new());
    let store = Arc::new(acct.as_tenant(T));
    let e = engine(&store, T, 1).await;
    registered(&e).await;
    let codes = Key::new(e.head_for_test().await.externals["x"].codes.clone());
    acct.record_ranges();
    let before = (acct.count(T, OpClass::Read), acct.count(T, OpClass::Write));

    // Off: no request at all.
    let off = Engine::new(Arc::clone(&store), T, LaneId(2));
    let err = off.copy_external("y", local(T), &*store).await.unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Off)),
        "{err}"
    );
    assert_eq!(
        (acct.count(T, OpClass::Read), acct.count(T, OpClass::Write)),
        before
    );

    // On, but not for the customer's bucket the registration names: the codes are not read.
    let narrow = Engine::new(Arc::clone(&store), T, LaneId(2))
        .with_external_sources(vec!["elsewhere/".to_owned()]);
    let err = narrow
        .copy_external("y", local(T), &*store)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Prefix(_))),
        "{err}"
    );
    assert!(
        acct.ranges().iter().all(|(k, _)| *k != codes),
        "the codes were read"
    );
    assert!(!narrow.head_for_test().await.externals.contains_key("y"));
}

#[tokio::test]
async fn a_copy_is_refused_with_its_reason() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, T, 1).await;
    let refused = |err: EngineError| match err {
        EngineError::External(r) => r,
        other => panic!("not an external refusal: {other}"),
    };
    // No such source index.
    let r = refused(e.copy_external("y", local(T), &*store).await.unwrap_err());
    assert_eq!(r, ExternalRefusal::NotFound("x".to_owned()));
    registered(&e).await;
    // A name taken, by the source itself and by a copy.
    let r = refused(e.copy_external("x", local(T), &*store).await.unwrap_err());
    assert_eq!(r, ExternalRefusal::Exists("x".to_owned()));
    e.copy_external("y", local(T), &*store).await.unwrap();
    let r = refused(e.copy_external("y", local(T), &*store).await.unwrap_err());
    assert_eq!(r, ExternalRefusal::Exists("y".to_owned()));
    // A name that cannot be one.
    for bad in ["", "..", "a/b"] {
        let err = e.copy_external(bad, local(T), &*store).await.unwrap_err();
        assert!(matches!(err, EngineError::Refused(_)), "{bad:?}: {err}");
    }
    // A source index named so it cannot exist.
    let err = e
        .copy_external("w", ReplicaSource::local(T, ".."), &*store)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Refused(_)), "{err}");
    // A codes blob that is not one.
    let codes = e.head_for_test().await.externals["x"].codes.clone();
    store
        .put(&Key::new(codes), b"not a codes blob".to_vec().into())
        .await
        .unwrap();
    let r = refused(e.copy_external("w", local(T), &*store).await.unwrap_err());
    assert!(matches!(r, ExternalRefusal::Source(_)), "{r}");
    assert!(!e.head_for_test().await.externals.contains_key("w"));
    // A source store that fails.
    let down = Flaky::refusing_reads();
    let err = e.copy_external("w", local(T), &down).await.unwrap_err();
    assert!(matches!(err, EngineError::Blob(_)), "{err}");
    assert!(!e.head_for_test().await.externals.contains_key("w"));
}

#[tokio::test]
async fn a_copy_outlives_its_source_and_buries_only_its_own_codes() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, T, 1).await;
    registered(&e).await;
    let want = e.query_external("x", &row(7), 10, 40).await.unwrap();
    e.copy_external("y", local(T), &*store).await.unwrap();
    let head = e.head_for_test().await;
    let (cx, cy) = (
        head.externals["x"].codes.clone(),
        head.externals["y"].codes.clone(),
    );

    // Dropping the source: the copy answers, and GC reaps the source's codes alone.
    e.drop_external("x").await.unwrap().unwrap();
    e.gc(0).await.unwrap();
    assert!(store.head(&Key::new(cx)).await.is_err());
    assert!(store.head(&Key::new(cy.clone())).await.is_ok());
    let got = e.query_external("y", &row(7), 10, 40).await.unwrap();
    assert_eq!(bits(&got), bits(&want));

    // Dropping the copy buries its own.
    let dropped = e.drop_external("y").await.unwrap().unwrap();
    assert!(e.head_for_test().await.graveyard[&dropped.0].contains(&cy));
    e.gc(0).await.unwrap();
    assert!(store.head(&Key::new(cy)).await.is_err());
    assert!(store.head(&Key::new(SOURCE)).await.is_ok());
}
