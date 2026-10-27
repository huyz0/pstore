//! M141: a tenant's buffered rows are counted exactly, and reserved under one lock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::MemoryStore;
use pstore_engine::{BufferBounds, BufferRefusal, Buffered, Engine};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

fn engine() -> Engine<MemoryStore> {
    Engine::new(Arc::new(MemoryStore::new()), TenantId(141), LaneId(1))
}

#[tokio::test]
async fn buffering_charges_each_row_and_a_flush_moves_exactly_its_rows() {
    let e = engine();
    let f = doc("d01").footprint();
    e.write("x", vec![doc("d01"), doc("d02")]).await.unwrap();
    assert_eq!(e.buffered().pending, 2 * f);
    e.flush().await.unwrap();
    let after = e.buffered();
    assert_eq!(after.pending, 0);
    assert!(after.unfolded > 0, "{after:?}");
    e.fold().await.unwrap();
    assert_eq!(e.buffered(), Buffered::default());
}

#[tokio::test]
async fn rows_buffered_during_a_flush_stay_charged() {
    let e = engine();
    let f = doc("d01").footprint();
    e.write("x", vec![doc("d01"), doc("d02")]).await.unwrap();
    // Written after the bundle landed and before its rows leave `pending`.
    e.flush_with_interference_for_test(async {
        e.write("x", vec![doc("d03")]).await.unwrap();
        e.write("y", vec![doc("d04")]).await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(e.buffered().pending, 2 * f);
    let mut left = e.pending_for_test().await;
    left.sort();
    assert_eq!(left, ["d03", "d04"]);
}

#[tokio::test]
async fn dropping_an_index_returns_its_pending_charge() {
    let e = engine();
    let f = doc("d01").footprint();
    e.write("x", vec![doc("d01")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("x", vec![doc("d02"), doc("d03")]).await.unwrap();
    e.write("y", vec![doc("d04")]).await.unwrap();
    assert_eq!(e.buffered().pending, 3 * f);
    assert!(e.delete_index("x").await.unwrap().is_some());
    assert_eq!(e.buffered().pending, f, "only y's row is still pending");
}

#[tokio::test]
async fn reservations_are_checked_and_charged_in_one_step() {
    let e = engine();
    let bounds = BufferBounds {
        flush_at: Some(100),
        cap: 150,
    };
    // Exactly at the flush bound fits; one byte over asks for a flush.
    let a = e.reserve_buffer(60, bounds).unwrap();
    let b = e.reserve_buffer(40, bounds).unwrap();
    assert_eq!(e.buffered().reserved, 100);
    assert_eq!(
        e.reserve_buffer(1, bounds).unwrap_err(),
        BufferRefusal::Flush
    );
    // Without a flush bound, the cap: exactly at it fits, one byte over is full.
    let unbounded = BufferBounds {
        flush_at: None,
        ..bounds
    };
    let c = e.reserve_buffer(50, unbounded).unwrap();
    assert_eq!(
        e.reserve_buffer(1, unbounded).unwrap_err(),
        BufferRefusal::Full
    );
    // A reservation dropped, on any path, returns its bytes, and nothing else.
    drop(b);
    assert_eq!(e.buffered().reserved, 110);
    assert!(e.reserve_buffer(40, unbounded).is_ok());
    drop((a, c));
    assert_eq!(e.buffered(), Buffered::default());
}

#[tokio::test]
async fn unfolded_bundles_count_against_the_cap_until_folded() {
    let e = engine();
    e.write("x", vec![doc("d01")]).await.unwrap();
    e.flush().await.unwrap();
    let u = e.buffered().unfolded;
    e.write("x", vec![doc("d02")]).await.unwrap();
    let f = doc("d02").footprint();
    let at = BufferBounds {
        flush_at: None,
        cap: u + f + 5,
    };
    // Unfolded plus pending plus the new reservation: exactly at the cap fits.
    let held = e.reserve_buffer(5, at).unwrap();
    drop(held);
    assert_eq!(e.reserve_buffer(6, at).unwrap_err(), BufferRefusal::Full);
    e.fold().await.unwrap();
    assert_eq!(e.buffered().unfolded, 0);
    assert!(e.reserve_buffer(u + 5, at).is_ok());
}

#[tokio::test]
async fn a_reservation_converts_into_the_rows_its_scope_buffers() {
    let e = engine();
    let f = doc("d01").footprint();
    let bounds = BufferBounds {
        flush_at: None,
        cap: 1000,
    };
    let r = e.reserve_buffer(f + 10, bounds).unwrap();
    r.scope(e.write("x", vec![doc("d01")])).await.unwrap();
    // The row moved its own footprint from the reservation: counted once, not twice.
    assert_eq!(
        e.buffered(),
        Buffered {
            pending: f,
            reserved: 10,
            unfolded: 0
        }
    );
    // A write outside the scope converts nothing.
    e.write("x", vec![doc("d02")]).await.unwrap();
    assert_eq!(e.buffered().reserved, 10);
    // Rows past what was reserved are charged as rows: the reservation is trued up.
    r.scope(e.write("x", vec![doc("d03")])).await.unwrap();
    assert_eq!(
        e.buffered(),
        Buffered {
            pending: 3 * f,
            reserved: 0,
            unfolded: 0
        }
    );
    drop(r);
    assert_eq!(e.buffered().reserved, 0);
    // A scope converts only its own reservation, and dropping either returns only its own.
    let a = e.reserve_buffer(5, bounds).unwrap();
    let b = e.reserve_buffer(f + 7, bounds).unwrap();
    b.scope(e.write("y", vec![doc("d04")])).await.unwrap();
    assert_eq!(e.buffered().reserved, 12);
    drop(a);
    assert_eq!(e.buffered().reserved, 7);
    drop(b);
    assert_eq!(e.buffered().reserved, 0);
    assert_eq!(e.buffered().pending, 4 * f);
}

#[tokio::test]
async fn an_applied_rows_stamp_is_what_stamp_footprint_says() {
    let e = engine();
    let f = doc("d01").footprint();
    let op = pstore_engine::OperationId(0x141);
    assert_eq!(pstore_engine::stamp_footprint(None, false, None), 0);
    let stamp = pstore_engine::stamp_footprint(Some(op), true, None);
    pstore_engine::stamped(Some(op), true, None, e.write("x", vec![doc("d01")]))
        .await
        .unwrap();
    assert_eq!(e.buffered().pending, f + stamp);
    // `$op` (3 + 8) and `$req` (4 + 32 hex digits + `:ids`).
    assert_eq!(stamp, 11 + 4 + 32 + 4);
}
