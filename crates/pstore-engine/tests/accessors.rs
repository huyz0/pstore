//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What a caller outside this crate may ask about HEAD — M7c.0.
//!
//! ⚠️ `head::read` is `pub(crate)`, so until now the only public door onto a tenant's index
//! list was `head_for_test`. A server needs the real one, and it needs it to cost one read.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(i: usize) -> Document {
    Document::new(format!("d{i}"), vec![i as f32, 0.5, -0.25, 1.0])
}

/// ⚠️ **The union is the point.** An index written a moment ago is queryable through the
/// freshness layer, so a route deciding existence on HEAD alone would `404` a document it
/// can return. These accessors report the two halves separately and the caller unions them,
/// because only the caller knows whether it is answering for this process or for the tenant.
#[tokio::test]
async fn the_index_accessors_read_head_once_and_see_the_unfolded_separately() {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(TenantId(3)));
    let e = Engine::new(Arc::clone(&store), TenantId(3), LaneId(1));

    assert!(
        e.indexes().await.unwrap().is_empty(),
        "a fresh tenant owns no index"
    );
    assert!(e.index_stats("nothing").await.unwrap().is_none());

    e.write("fresh", vec![doc(0), doc(1)]).await.unwrap();
    assert_eq!(e.pending_indexes().await, vec!["fresh".to_owned()]);
    assert!(
        e.indexes().await.unwrap().is_empty(),
        "HEAD names only folded indexes"
    );

    // ⚠️ **Flushed is still unfolded**, and the memtable holds those rows in a second map.
    // Reporting only the buffered half would leave an index durable, queryable, and absent
    // from the list of indexes that exist -- and a fixture that never flushes cannot see it.
    e.flush().await.unwrap();
    assert_eq!(
        e.pending_indexes().await,
        vec!["fresh".to_owned()],
        "a flushed but unfolded index vanished from the unfolded list"
    );
    assert!(e.indexes().await.unwrap().is_empty());

    e.fold().await.unwrap();
    assert_eq!(e.indexes().await.unwrap(), vec!["fresh".to_owned()]);
    assert!(
        e.pending_indexes().await.is_empty(),
        "the fold emptied the memtable"
    );

    // ⚠️ A **second** segment, because `segments: refs.len()` and `segments: 1` are the same
    // number for a one-segment index -- measured: the mutant survived the first fixture.
    e.write("fresh", vec![doc(2)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let before = acct.count(TenantId(3), OpClass::Read);
    let stats = e
        .index_stats("fresh")
        .await
        .unwrap()
        .expect("the index was just folded");
    assert_eq!(
        acct.count(TenantId(3), OpClass::Read) - before,
        1,
        "index_stats must be ONE read of HEAD, never a walk of its segments"
    );
    assert_eq!(
        (stats.segments, stats.documents),
        (2, 3),
        "two segments holding three rows between them"
    );
    assert_eq!(stats.epoch, e.epoch());
    assert_eq!(
        acct.count(TenantId(3), OpClass::List),
        0,
        "an accessor listed"
    );
}
