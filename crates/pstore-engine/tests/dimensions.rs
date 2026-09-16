//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! One index, one width — M7c, after code review.
//!
//! ⚠️ **Found through the API and measured by a reviewer**: a four-dimensional document and a
//! two-dimensional one written to the same index in **separate batches** were both accepted,
//! and the short one then outranked an exact match. The server's door check compared
//! dimensions *within* a request, which is the case that never mattered.
//!
//! An index's vectors must all be the same width — `api-design.md` says so — and the cheapest
//! place to enforce it is where the rows already are.

use pstore_blob::MemoryStore;
use pstore_engine::{Engine, EngineError};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(5);

#[tokio::test]
async fn a_second_batch_may_not_change_the_index_width() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    e.write("docs", vec![Document::new("a", vec![1.0, 0.5, -0.25, 1.0])])
        .await
        .unwrap();

    // ⚠️ A separate `write`, which is how a client does it: the batch is consistent with
    // itself and inconsistent with the index.
    let err = e
        .write("docs", vec![Document::new("b", vec![9.0, 9.0])])
        .await
        .expect_err("a two-dimensional document joined a four-dimensional index");
    assert!(
        matches!(
            err,
            EngineError::DimensionMismatch {
                expected: 4,
                got: 2
            }
        ),
        "{err:?}"
    );

    // The right width still lands, so the check refuses only what it should.
    e.write("docs", vec![Document::new("c", vec![2.0, 0.5, -0.25, 1.0])])
        .await
        .unwrap();

    // ⚠️ And a **different index** of the same tenant keeps its own width: the rule is per
    // index, which is the unit of schema.
    e.write("other", vec![Document::new("d", vec![1.0, 2.0])])
        .await
        .unwrap();
}

#[tokio::test]
async fn the_width_survives_a_flush_and_a_fold() {
    // ⚠️ The case the memtable alone cannot answer, and the one review's scenario used: the
    // rows are gone from `pending` by the time the second batch arrives.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![Document::new("a", vec![1.0, 0.5, -0.25, 1.0])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    let err = e
        .write("docs", vec![Document::new("b", vec![9.0, 9.0])])
        .await
        .expect_err("a flushed index accepted a different width");
    assert!(
        matches!(err, EngineError::DimensionMismatch { .. }),
        "{err:?}"
    );

    e.fold().await.unwrap();
    // ⚠️ After the fold the memtable is empty and the width lives in the segment. A process
    // that has just started knows nothing either -- so this is the case that must be **loud
    // at query time** rather than silently wrong, and `a_query_vector_of_the_wrong_dimension`
    // in `pstore-query` is what makes it so.
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(2));
    fresh
        .write("docs", vec![Document::new("b", vec![9.0, 9.0])])
        .await
        .expect("a new process cannot know the width without a read, and must not pay one");
}
