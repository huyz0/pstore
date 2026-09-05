//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The round-trip budget is a tested invariant (D-34), not a design claim, so first the
//! counter itself has to be shown to measure DEPTH rather than request count.
use bytes::Bytes;
use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_testkit::depth::DepthCounting;

fn k(s: &str) -> Key {
    Key::new(s)
}

#[tokio::test]
async fn a_loop_that_awaits_each_result_counts_every_iteration() {
    let s = DepthCounting::new(MemoryStore::new());
    s.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    s.reset();
    for _ in 0..4 {
        s.get(&k("a")).await.unwrap();
    }
    assert_eq!(s.depth(), 4);
    assert_eq!(s.requests(), 4);
}

#[tokio::test]
async fn concurrent_fan_out_counts_once() {
    // The property the whole budget rests on: width is free, depth is not.
    let s = std::sync::Arc::new(DepthCounting::new(MemoryStore::new()));
    s.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    s.reset();
    let key = k("a");
    let futs: Vec<_> = (0..8).map(|_| s.get(&key)).collect();
    for f in futures_util::future::join_all(futs).await {
        f.unwrap();
    }
    assert_eq!(s.requests(), 8);
    assert_eq!(s.depth(), 1, "eight concurrent reads are one round trip");
}

#[tokio::test]
async fn a_failed_request_still_counts() {
    // Depth is what the caller waited for, not what succeeded.
    let s = DepthCounting::new(MemoryStore::new());
    s.reset();
    assert!(s.get(&k("absent")).await.is_err());
    assert_eq!(s.depth(), 1);
}

#[tokio::test]
async fn get_ranges_fans_out_rather_than_looping() {
    // Coalescing merges NEARBY ranges; ranges far apart stay separate fetches, and those
    // must be issued together. A `for` loop awaiting each one turns a scan of forty
    // blocks into forty sequential round trips -- which is a 1.2-second cold query at
    // 30 ms a hop, and invisible to every functional test.
    let s = DepthCounting::new(MemoryStore::new());
    s.put(&k("o"), Bytes::from(vec![7u8; 1_000_000]))
        .await
        .unwrap();
    s.reset();
    // Eight ranges, each 100 KiB apart: far beyond the 64 KiB coalescing gap.
    let ranges: Vec<_> = (0..8u64)
        .map(|i| (i * 100_000)..(i * 100_000 + 16))
        .collect();
    let out = s.get_ranges(&k("o"), &ranges).await.unwrap();
    assert_eq!(out.len(), 8);
    assert_eq!(
        s.requests(),
        8,
        "they cannot be coalesced, so eight requests"
    );
    assert_eq!(s.depth(), 1, "but they must cost ONE round trip");
}
