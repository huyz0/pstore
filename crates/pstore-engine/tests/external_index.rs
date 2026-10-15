//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M86: an external index registered by name, built by the engine, named by HEAD, queried by
//! name, and dropped through the graveyard.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_engine::{Engine, EngineError, ExternalKind, ExternalRefusal};
use pstore_format::Document;
use pstore_format::external::ExternalColumn;
use pstore_index::external::ExternalIndex;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
const DIMS: usize = 8;
const T: TenantId = TenantId(86);

async fn lake(store: &impl BlobStore) {
    let raw = std::fs::read(format!("{DATA}good.parquet")).unwrap();
    store
        .put(&Key::new("lake/good.parquet"), raw.into())
        .await
        .unwrap();
}

/// A Lance dataset's files, under `lake/<name>/`.
async fn lance(store: &impl BlobStore, name: &str) {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let root = std::path::PathBuf::from(format!("{DATA}lance/{name}"));
    let mut files = Vec::new();
    walk(&root, &mut files);
    for f in files {
        let rel = f.strip_prefix(&root).unwrap().to_str().unwrap().to_owned();
        store
            .put(
                &Key::new(format!("lake/{name}/{rel}")),
                std::fs::read(&f).unwrap().into(),
            )
            .await
            .unwrap();
    }
}

fn vectors(name: &str) -> Vec<Vec<f32>> {
    std::fs::read(format!("{DATA}{name}"))
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect::<Vec<_>>()
        .chunks_exact(DIMS)
        .map(<[f32]>::to_vec)
        .collect()
}

fn allowed(store: &Arc<MemoryStore>) -> Engine<MemoryStore> {
    Engine::new(Arc::clone(store), T, LaneId(1)).with_external_sources(vec!["lake/".to_owned()])
}

fn bits(hits: &[(usize, f32)]) -> Vec<(usize, u32)> {
    hits.iter().map(|(r, s)| (*r, s.to_bits())).collect()
}

#[tokio::test]
async fn register_then_query_by_name_equals_the_library() {
    let store = Arc::new(MemoryStore::new());
    lake(&*store).await;
    lance(&*store, "one").await;
    let e = allowed(&store);
    let vs = vectors("good.vectors.f32");
    for (kind, source, column) in [
        (ExternalKind::Parquet, "lake/good.parquet", "embedding"),
        (ExternalKind::Lance, "lake/one", "emb"),
    ] {
        let name = format!("{kind:?}");
        let epoch = e
            .register_external(&name, kind, source, column)
            .await
            .unwrap();
        assert!(epoch.0 > 0);
        // The library, over the same column.
        let col = match kind {
            ExternalKind::Parquet => {
                ExternalColumn::pin_parquet(&*store, &Key::new(source), column).await
            }
            ExternalKind::Lance => ExternalColumn::pin_lance_latest(&*store, source, column).await,
        }
        .unwrap();
        let lib = ExternalIndex::build(&*store, &Key::new("scratch/lib.codes"), col, 64)
            .await
            .unwrap();
        for q in [&vs[0], &vs[17], &vs[250]] {
            let want = lib.search(&*store, q, 10, 40).await.unwrap();
            let got = e.query_external(&name, q, 10, 40).await.unwrap();
            assert_eq!(bits(&got), bits(&want), "{name}");
            assert_eq!(got.len(), 10);
        }
    }
    // HEAD names both, and another handle on the tenant answers the same.
    let head = e.head_for_test().await;
    assert_eq!(head.externals.len(), 2);
    let other = Engine::new(Arc::clone(&store), T, LaneId(2))
        .with_external_sources(vec!["lake/".to_owned()]);
    let a = e.query_external("Parquet", &vs[3], 5, 20).await.unwrap();
    let b = other
        .query_external("Parquet", &vs[3], 5, 20)
        .await
        .unwrap();
    assert_eq!(bits(&a), bits(&b));
}

#[tokio::test]
async fn an_unregistered_source_is_refused_unless_allowed() {
    let acct = Accounted::new(MemoryStore::new());
    let store = Arc::new(acct.as_tenant(T));
    lake(&*store).await;
    acct.record_ranges();
    let before = (acct.count(T, OpClass::Read), acct.count(T, OpClass::Write));
    // The default: off.
    let off = Engine::new(Arc::clone(&store), T, LaneId(1));
    let err = off
        .register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Off)),
        "{err}"
    );
    // Allowed, but not this prefix.
    let narrow = Engine::new(Arc::clone(&store), T, LaneId(1))
        .with_external_sources(vec!["elsewhere/".to_owned()]);
    let err = narrow
        .register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Prefix(_))),
        "{err}"
    );
    // Allowed with no prefix at all is still off: nothing matches an empty allowlist.
    let none = Engine::new(Arc::clone(&store), T, LaneId(1)).with_external_sources(Vec::new());
    let err = none
        .register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Off)),
        "{err}"
    );
    // And a query by name is refused the same way: no byte of the source, no request at all.
    let err = off
        .query_external("x", &[1.0; DIMS], 5, 10)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Off)),
        "{err}"
    );
    assert!(acct.ranges().is_empty(), "{:?}", acct.ranges());
    assert_eq!(
        (acct.count(T, OpClass::Read), acct.count(T, OpClass::Write)),
        before,
        "a refused registration made a request"
    );
}

#[tokio::test]
async fn a_changed_source_is_an_error_not_a_stale_answer() {
    let store = Arc::new(MemoryStore::new());
    lake(&*store).await;
    let e = allowed(&store);
    e.register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap();
    let vs = vectors("good.vectors.f32");
    assert!(e.query_external("x", &vs[0], 5, 20).await.is_ok());
    // The customer rewrites the file.
    let mut raw = std::fs::read(format!("{DATA}good.parquet")).unwrap();
    raw[4] ^= 0xFF;
    store
        .put(&Key::new("lake/good.parquet"), raw.into())
        .await
        .unwrap();
    let err = e.query_external("x", &vs[0], 5, 20).await.unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Changed)),
        "{err}"
    );
}

#[tokio::test]
async fn dropping_a_registration_buries_its_codes() {
    let store = Arc::new(MemoryStore::new());
    lake(&*store).await;
    let e = allowed(&store);
    e.register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap();
    let codes = e.head_for_test().await.externals["x"].codes.clone();
    assert!(store.head(&Key::new(codes.clone())).await.is_ok());
    // A name that is not registered drops nothing and commits nothing.
    let epoch = e.head_for_test().await.epoch;
    assert_eq!(e.drop_external("nope").await.unwrap(), None);
    assert_eq!(e.head_for_test().await.epoch, epoch);
    let dropped = e.drop_external("x").await.unwrap().unwrap();
    let head = e.head_for_test().await;
    assert!(head.externals.is_empty());
    assert!(
        head.graveyard
            .get(&dropped.0)
            .is_some_and(|keys| keys.contains(&codes)),
        "{:?}",
        head.graveyard
    );
    // Still there for a reader of the epoch before, until GC passes it; the customer's file is
    // never touched.
    assert!(store.head(&Key::new(codes.clone())).await.is_ok());
    let err = e
        .query_external("x", &[1.0; DIMS], 5, 10)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::NotFound(_))),
        "{err}"
    );
    e.gc(0).await.unwrap();
    assert!(
        store.head(&Key::new(codes)).await.is_err(),
        "GC left the codes"
    );
    assert!(store.head(&Key::new("lake/good.parquet")).await.is_ok());
}

#[tokio::test]
async fn a_registration_the_engine_cannot_honour_is_refused_with_its_reason() {
    let store = Arc::new(MemoryStore::new());
    lake(&*store).await;
    let e = allowed(&store);
    let bad = |err: EngineError, why: &str| {
        assert!(
            matches!(&err, EngineError::External(ExternalRefusal::Source(m)) if m.contains(why)),
            "wanted {why:?}, got {err}"
        );
    };
    bad(
        e.register_external("x", ExternalKind::Parquet, "lake/good.parquet", "nope")
            .await
            .unwrap_err(),
        "no column",
    );
    bad(
        e.register_external(
            "x",
            ExternalKind::Parquet,
            "lake/missing.parquet",
            "embedding",
        )
        .await
        .unwrap_err(),
        "",
    );
    // Nothing was committed by any refusal.
    assert!(e.head_for_test().await.externals.is_empty());
    // A name taken by a registration, or by an index with rows.
    e.register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap();
    let err = e
        .register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Exists(_))),
        "{err}"
    );
    e.write("docs", vec![Document::new("a", vec![1.0, 0.0])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let err = e
        .register_external(
            "docs",
            ExternalKind::Parquet,
            "lake/good.parquet",
            "embedding",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Exists(_))),
        "{err}"
    );
    // A query of the wrong width is the library's refusal, by name.
    let err = e.query_external("x", &[1.0, 0.0], 5, 10).await.unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Source(_))),
        "{err}"
    );
}
