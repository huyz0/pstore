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
    // M213: the cap scaled to the row's held bytes; it was 1,000 when a row was charged 15.
    let bounds = BufferBounds {
        flush_at: None,
        cap: 10 * f,
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
    // M213: a map leaf for a row that had no attributes, then `$op` (3), `$req` (4) and its
    // 32 hex digits and `:ids`, each one allocation.
    let a = pstore_format::ALLOCATION_OVERHEAD;
    let leaf = pstore_format::map_bytes::<String, pstore_format::Value>(1);
    assert_eq!(stamp, leaf + (3 + a) + (4 + a) + (32 + 4 + a));
}

/// M213: a flushed row stays in memory until a fold, and is charged what it was pending.
#[tokio::test]
async fn a_flushed_row_keeps_its_held_charge_until_folded() {
    let e = engine();
    let rows = vec![doc("d01"), doc("d02"), doc("d03")];
    e.write("x", rows).await.unwrap();
    let pending = e.buffered().pending;
    assert_eq!(pending, 3 * doc("d01").footprint());
    e.flush().await.unwrap();
    assert_eq!(
        e.buffered(),
        Buffered {
            pending: 0,
            reserved: 0,
            unfolded: pending
        }
    );
    e.fold().await.unwrap();
    assert_eq!(e.buffered(), Buffered::default());
}

#[tokio::test]
async fn each_operations_estimate_is_the_row_the_engine_buffers() {
    // M213: the server reserves a write by these before anything is buffered, so each must be
    // what buffering it charges -- or the reservation is left over, or the row unchecked.
    let patch = pstore_engine::Patch {
        id: "p01".to_owned(),
        set: [(
            "colour".to_owned(),
            pstore_format::Value::Str("blue".to_owned()),
        )]
        .into(),
        unset: vec!["size".to_owned()],
    };
    let by = pstore_engine::Patch {
        id: String::new(),
        set: [("n".to_owned(), pstore_format::Value::Int(1))].into(),
        unset: Vec::new(),
    };
    let filter = pstore_query::Predicate::Absent("owner".to_owned());
    let charged = |e: &Engine<MemoryStore>| e.buffered().pending;

    let e = engine();
    e.delete("x", vec!["gone1".to_owned()]).await.unwrap();
    assert_eq!(charged(&e), pstore_engine::delete_footprint("gone1"));
    let e = engine();
    e.patch("x", vec![patch.clone()], None).await.unwrap();
    assert_eq!(charged(&e), pstore_engine::patch_footprint(&patch));
    let e = engine();
    e.delete_by_filter("x", &filter).await.unwrap();
    assert_eq!(
        charged(&e),
        pstore_engine::by_filter_footprint(&filter, None)
    );
    let e = engine();
    e.patch_by_filter("x", &filter, by.clone()).await.unwrap();
    assert_eq!(
        charged(&e),
        pstore_engine::by_filter_footprint(&filter, Some(&by))
    );
    // Each a different row, so an estimate standing in for another is caught.
    let all = [
        pstore_engine::delete_footprint("gone1"),
        pstore_engine::patch_footprint(&patch),
        pstore_engine::by_filter_footprint(&filter, None),
        pstore_engine::by_filter_footprint(&filter, Some(&by)),
    ];
    for (i, a) in all.iter().enumerate() {
        assert!(all[..i].iter().all(|b| b != a), "{all:?}");
    }
    // A patch the engine refuses holds nothing.
    let bad = pstore_engine::Patch {
        id: "p02".to_owned(),
        set: [("$op".to_owned(), pstore_format::Value::Int(1))].into(),
        unset: Vec::new(),
    };
    assert_eq!(pstore_engine::patch_footprint(&bad), 0);
    assert_eq!(pstore_engine::by_filter_footprint(&filter, Some(&bad)), 0);
}
