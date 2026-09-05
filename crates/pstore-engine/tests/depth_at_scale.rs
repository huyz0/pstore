//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(clippy::unwrap_used, clippy::panic, reason = "assertions report")]

//! Does the ≤3 round-trip invariant survive a segment big enough to matter?

use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(i: usize) -> Document {
    Document {
        id: format!("d{i}"),
        vector: vec![0.0; 4],
        attrs: Default::default(),
    }
}

#[tokio::test]
async fn a_cold_read_of_a_large_segment_still_costs_three_rounds() {
    // M1 asserts ≤3 on a 500-row fixture. 500 rows is 8 blocks, whose index section fits
    // the 8 KiB suffix read -- so the segment opens in ONE round and the invariant holds
    // for a reason the test never states. This asks the same question at a size where the
    // index section cannot fit.
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let t = TenantId(1);
    let e = Engine::new(Arc::clone(&s), t, LaneId(0));
    for chunk in 0..40 {
        e.write("idx", (0..1000).map(|i| doc(chunk * 1000 + i)).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
    }
    e.fold().await.unwrap();

    let cold = Engine::new(Arc::clone(&s), t, LaneId(9));
    s.reset();
    let got = cold.scan("idx", None).await.unwrap();
    assert_eq!(got.len(), 40_000);
    assert!(
        s.depth() <= 3,
        "a 40,000-row segment took {} sequential round trips",
        s.depth()
    );
}

#[tokio::test]
async fn the_block_index_stays_inside_the_suffix_read_at_any_size() {
    // The invariant stated directly, rather than inferred from a depth count: whatever the
    // row count, the index section fits the one read that opens the segment. Depth is the
    // consequence; this is the cause, and it is the thing a future change to block sizing
    // would break first.
    for rows in [1usize, 100, 5_000, 60_000] {
        let mut w = pstore_format::SegmentWriter::new(64);
        for i in 0..rows {
            w.push(doc(i));
        }
        let seg = w.finish();
        let idx_len = pstore_format::index_section_len(&seg).unwrap();
        assert!(
            idx_len <= pstore_format::INDEX_BUDGET,
            "{rows} rows produced a {idx_len}-byte index section, over the \
             {}-byte budget: the segment now opens in two round trips",
            pstore_format::INDEX_BUDGET
        );
    }
}

#[tokio::test]
async fn bounding_the_index_does_not_just_move_the_cost_into_bytes() {
    // ⚠️ The obvious wrong fix. Growing the block size until the index fits is only a win
    // if a scan still reads roughly the data and not much else -- a segment of one enormous
    // block has a tiny index and forces every filtered read to drag in everything.
    let s = Arc::new(pstore_blob::Accounted::new(MemoryStore::new()));
    let t = TenantId(2);
    let v = Arc::new(s.as_tenant(t));
    let e = Engine::new(Arc::clone(&v), t, LaneId(0));
    for chunk in 0..40 {
        e.write("idx", (0..1000).map(|i| doc(chunk * 1000 + i)).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
    }
    e.fold().await.unwrap();

    let before = s.bytes(t, pstore_blob::OpClass::Read);
    let got = e.scan("idx", None).await.unwrap();
    let read = s.bytes(t, pstore_blob::OpClass::Read) - before;
    assert_eq!(got.len(), 40_000);
    // A full scan must read the segment about once. Twice would mean the index is being
    // re-fetched or blocks are overlapping.
    let stored = s.bytes(t, pstore_blob::OpClass::Write);
    assert!(
        read < stored * 2,
        "a full scan moved {read} bytes over {stored} bytes stored"
    );
}
