//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A cold indexed query, from the tenant id alone — M5g.
//!
//! ⚠️ **Moved here from `pstore-index`, and the comment it arrived with is now wrong.** It
//! said the joining layer "is `pstore-query` (layer 4) and does not exist yet, so this test
//! does the joining -- legitimately, because `pstore-index` is layer 3 and may depend DOWNWARD
//! on `pstore-engine`." That ordering stopped holding the moment the **fold** had to build a
//! dense index: `pstore-engine` now depends on `pstore-index` and `pstore-query`, and is the
//! composition point rather than a layer beneath them. The dev-dependency that pointed the
//! other way went with this test.
//!
//! ⚠️ And it is a stronger test here: the segment is one the **engine produced**, not one a
//! fixture hand-built and a hand-written HEAD named.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_index::vec_index::{self, Query, VecIndex};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const DIM: usize = 12;
/// Small enough that the test does not have to write 25,000 rows to reach the clustered path.
const TEST_THRESHOLD: usize = 100;

fn corpus(n: usize) -> Vec<Document> {
    (0..n)
        .map(|i| {
            Document::new(
                format!("d{i:05}"),
                (0..DIM)
                    .map(|j| ((i * 7 + j * 13) % 97) as f32 / 97.0)
                    .collect(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_cold_query_from_head_costs_three_round_trips() {
    // The real sequence a client experiences:
    //
    //   1. read HEAD, which names the segment
    //   2. the segment footer and the centroid object, together -- different objects, both
    //      keys derived, so width rather than depth
    //   3. the probed posting lists, rabitq and sq8 ranges in one call
    //
    // Three. `rerank: fast` is inside them; only `exact` adds a fourth, and it says so.
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let t = TenantId(77);
    let docs = corpus(TEST_THRESHOLD + 400);

    let e = Engine::new(Arc::clone(&s), t, LaneId(0)).with_index_params(
        pstore_index::cluster::Params {
            target_list_size: 40,
            exact_scan_threshold: TEST_THRESHOLD,
            ..pstore_index::cluster::Params::default()
        },
    );
    e.write("idx", docs.clone()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    // Cold: nothing cached, starting from the tenant id alone.
    s.reset();
    let head = e.head_for_test().await;
    let seg_key = Key::new(head.indexes["idx"][0].key.clone());
    let idx = VecIndex::open(
        s.as_ref(),
        &seg_key,
        &vec_index::centroid_key(&seg_key),
        DIM,
    )
    .await
    .unwrap();
    let hits = idx
        .search(s.as_ref(), &seg_key, docs[9].vector(), Query::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 10);
    assert_eq!(
        s.depth(),
        3,
        "a cold query from HEAD took {} sequential round trips",
        s.depth()
    );
    // ⚠️ And the centroid object is really there and really used: without it `VecIndex::open`
    // falls back to an exact scan (D-10) and the depth assertion above passes for the wrong
    // reason.
    assert!(
        s.get(&vec_index::centroid_key(&seg_key)).await.is_ok(),
        "the engine folded without writing a centroid table"
    );
}
