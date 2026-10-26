//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

//! When an index's contents last changed — M9f.1. The API has no compaction route, so the
//! compaction case is here.

use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{Epoch, LaneId, TenantId};
use std::sync::Arc;

#[tokio::test]
async fn a_compaction_moves_the_updated_epoch_and_another_indexs_fold_does_not() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(50), LaneId(1));
    let mut last = Epoch(0);
    for i in 0..2 {
        e.write("a", vec![Document::new(format!("d{i}"), vec![1.0])])
            .await
            .unwrap();
        e.flush().await.unwrap();
        last = e.fold().await.unwrap();
    }
    let updated = |s: Option<pstore_engine::IndexStats>| s.unwrap().updated_epoch;
    assert_eq!(updated(e.index_stats("a").await.unwrap()), Some(last));
    e.write("b", vec![Document::new("x", vec![1.0])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(
        updated(e.index_stats("a").await.unwrap()),
        Some(last),
        "another index's fold moved it"
    );
    let merged = e.compact("a").await.unwrap().expect("a merge");
    assert_eq!(updated(e.index_stats("a").await.unwrap()), Some(merged));
}
