//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Range coalescing: the primitive that converts round trips into bytes.
//!
//! Cheap GETs, free intra-region bandwidth and ~30 ms round trips mean the right bias is
//! to fetch a superset. `G*` is where the cost of transferring the gap equals the cost of
//! an extra request.
use bytes::Bytes;
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass, coalesce};
use pstore_types::TenantId;

fn k(s: &str) -> Key {
    Key::new(s)
}

#[tokio::test]
async fn coalescing_merges_ranges_closer_than_the_gap() {
    // Three ranges separated by 4-byte gaps, threshold 16: one request.
    let plan = coalesce(&[0..10, 14..20, 24..30], 16);
    assert_eq!(plan.len(), 1, "expected one merged fetch, got {plan:?}");
    assert_eq!(plan[0].span, 0..30);
}

#[tokio::test]
async fn coalescing_respects_the_gap_threshold() {
    // The same ranges with threshold 2: merging unconditionally would pull 30 bytes to
    // serve 22, and at real block sizes that is unbounded read amplification.
    let plan = coalesce(&[0..10, 14..20, 24..30], 2);
    assert_eq!(plan.len(), 3, "expected no merging, got {plan:?}");
}

#[tokio::test]
async fn coalescing_never_issues_more_requests_than_ranges() {
    for gap in [0, 1, 8, 64, 4096] {
        let plan = coalesce(&[0..4, 8..12, 100..104, 4096..4100], gap);
        assert!(plan.len() <= 4, "gap {gap} produced {} fetches", plan.len());
        assert!(!plan.is_empty());
    }
}

#[tokio::test]
async fn coalesced_reads_equal_separate_reads() {
    let s = MemoryStore::new();
    let body: Vec<u8> = (0..=255u8).collect();
    s.put(&k("o"), Bytes::from(body)).await.unwrap();
    let wanted = [0..4u64, 10..20, 22..25, 200..256];

    let together = s.get_ranges(&k("o"), &wanted).await.unwrap();
    let mut separate = Vec::new();
    for r in &wanted {
        separate.push(s.get_range(&k("o"), r.clone()).await.unwrap());
    }
    // Returning the merged buffer rather than each range's slice is the mutation this
    // catches, and it would be invisible to a length-only assertion.
    assert_eq!(together, separate);
    assert_eq!(&together[0][..], &[0, 1, 2, 3]);
    assert_eq!(together[3].len(), 56);
}

#[tokio::test]
async fn coalescing_actually_saves_requests() {
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let v = s.as_tenant(t);
    v.put(&k("o"), Bytes::from(vec![7u8; 4096])).await.unwrap();
    let before = s.count(t, OpClass::Read);
    // Eight ranges, each 8 bytes apart: one fetch.
    let ranges: Vec<_> = (0..8u64).map(|i| (i * 16)..(i * 16 + 8)).collect();
    let out = v.get_ranges(&k("o"), &ranges).await.unwrap();
    assert_eq!(out.len(), 8);
    assert_eq!(
        s.count(t, OpClass::Read) - before,
        1,
        "eight nearby ranges must cost one request, not eight"
    );
}

#[tokio::test]
async fn ranges_out_of_order_are_returned_in_the_order_asked() {
    let s = MemoryStore::new();
    s.put(&k("o"), Bytes::from((0..=255u8).collect::<Vec<_>>()))
        .await
        .unwrap();
    let out = s
        .get_ranges(&k("o"), &[200..204, 0..4, 100..104])
        .await
        .unwrap();
    assert_eq!(&out[0][..], &[200, 201, 202, 203]);
    assert_eq!(&out[1][..], &[0, 1, 2, 3]);
    assert_eq!(&out[2][..], &[100, 101, 102, 103]);
}
