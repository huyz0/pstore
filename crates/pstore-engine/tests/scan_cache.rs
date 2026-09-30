//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! D-50 under a read cache (M20): a compaction, a fold's delete pass and a full scan read
//! every block of what they touch, once, so they must not admit it. That a scan is still
//! served from bulk is `pstore-cache`'s `a_scan_is_served_from_bulk_and_admits_nothing`: a
//! query reads a section whole and a scan reads it by block, so no engine path shares a
//! range between them today (M20's amendment to criterion 9).

use pstore_blob::{Accounted, Class, MemoryStore, TenantView};
use pstore_cache::Caching;
use pstore_engine::{Engine, Patch};
use pstore_format::{Document, Value};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(200);

type Cached = Caching<TenantView<MemoryStore>>;

struct World {
    cache: Arc<Cached>,
    e: Engine<Cached>,
}

/// An index of two folded segments, read through a memory cache.
async fn world() -> World {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let cache = Arc::new(Caching::new(Arc::new(acct.as_tenant(T)), 64 << 20));
    let e = Engine::new(Arc::clone(&cache), T, LaneId(1));
    for part in 0..2 {
        let docs = (0..300)
            .map(|i| {
                let x = (part * 300 + i) as f32;
                Document::new(format!("d{part}-{i}"), vec![x.sin(), x.cos()])
            })
            .collect();
        e.write("idx", docs).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    World { cache, e }
}

impl World {
    fn bulk(&self) -> usize {
        self.cache.resident_in(Class::Bulk)
    }
}

#[tokio::test]
async fn a_cold_compaction_admits_no_bulk() {
    let w = world().await;
    let before = w.bulk();
    w.e.compact("idx").await.unwrap().unwrap();
    assert_eq!(w.bulk(), before, "a compaction's scan was admitted");
}

#[tokio::test]
async fn a_cold_scan_admits_no_bulk() {
    let w = world().await;
    let before = w.bulk();
    assert_eq!(w.e.scan("idx", None).await.unwrap().len(), 600);
    assert_eq!(w.bulk(), before, "a full scan was admitted");
}

#[tokio::test]
async fn a_folds_delete_pass_admits_no_bulk() {
    let w = world().await;
    let before = w.bulk();
    w.e.delete("idx", vec!["d0-1".into(), "d1-2".into()])
        .await
        .unwrap();
    w.e.flush().await.unwrap();
    w.e.fold().await.unwrap();
    assert_eq!(w.bulk(), before, "the fold's delete pass was admitted");
}

fn dense() -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.0],
        limit: 10,
        tune: Query {
            exact: true,
            ..Query::default()
        },
    }]
}

#[tokio::test]
async fn a_query_admits() {
    let w = world().await;
    let before = w.bulk();
    w.e.query("idx", &dense(), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(w.bulk() > before, "a query admitted nothing");
}

#[tokio::test]
async fn a_compaction_over_deleted_rows_admits_no_bulk() {
    // With a delete vector, the live rows are read by `rows_where`, not `scan`.
    let w = world().await;
    w.e.delete("idx", vec!["d0-1".into(), "d1-2".into()])
        .await
        .unwrap();
    w.e.flush().await.unwrap();
    w.e.fold().await.unwrap();
    let before = w.bulk();
    w.e.compact("idx").await.unwrap().unwrap();
    assert_eq!(w.bulk(), before, "a compaction's rows_where was admitted");
    assert_eq!(w.e.scan("idx", None).await.unwrap().len(), 598);
}

#[tokio::test]
async fn a_folds_patch_pass_admits_no_bulk() {
    // A patch is deferred: the fold reads the rows it may change through `scan`.
    let w = world().await;
    let before = w.bulk();
    let patch = Patch {
        id: "d0-1".to_owned(),
        set: std::collections::BTreeMap::from([("a".to_owned(), Value::Int(2))]),
        unset: vec![],
    };
    w.e.patch("idx", vec![patch], None).await.unwrap();
    w.e.flush().await.unwrap();
    w.e.fold().await.unwrap();
    assert_eq!(w.bulk(), before, "the fold's patch pass was admitted");
}
