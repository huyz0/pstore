//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A successor must find a predecessor's bundles from the blob store alone: no injected
//! watermark, no listing, no shared memory. Until this works, OQ-91 cannot even be asked.
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::{Engine, lanes};
use pstore_format::{Document, Value};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![n as f32]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

#[tokio::test]
async fn a_lane_registers_once_and_is_discoverable() {
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(1);
    lanes::register(&*s, t, LaneId(7)).await.unwrap();
    lanes::register(&*s, t, LaneId(9)).await.unwrap();
    let mut live = lanes::live(&*s, t).await.unwrap();
    live.sort();
    assert_eq!(live, vec![LaneId(7), LaneId(9)]);
}

#[tokio::test]
async fn a_lane_bitmap_records_a_lane_in_one_cas() {
    // Once per lane LIFETIME, not per write. A CAS per write would put the commit
    // protocol's ~5/s ceiling on the write path, which is the thing lanes exist to avoid.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(2);
    let v = s.as_tenant(t);

    let before = s.count(t, OpClass::Write);
    lanes::register(&v, t, LaneId(1)).await.unwrap();
    assert_eq!(
        s.count(t, OpClass::Write) - before,
        1,
        "first registration is one CAS"
    );

    let before = s.count(t, OpClass::Write);
    for _ in 0..20 {
        lanes::register(&v, t, LaneId(1)).await.unwrap();
    }
    assert_eq!(
        s.count(t, OpClass::Write) - before,
        0,
        "re-registering an already-live lane must cost nothing"
    );
}

#[tokio::test]
async fn concurrent_lane_registrations_all_survive() {
    // Two lanes registering at once must not lose one another: the CAS loser has to
    // rebase and re-add, not overwrite.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(3);
    let mut tasks = Vec::new();
    for i in 0..16u64 {
        let s = Arc::clone(&s);
        tasks.push(tokio::spawn(async move {
            lanes::register(&*s, t, LaneId(i)).await.unwrap();
        }));
    }
    for h in tasks {
        h.await.unwrap();
    }
    let live = lanes::live(&*s, t).await.unwrap();
    assert_eq!(live.len(), 16, "a registration was lost: {live:?}");
}

#[tokio::test]
async fn a_successor_finds_the_tail_by_probing_not_listing() {
    // Forward probing: the sequence numbers are dense and derived, so the tail is found by
    // asking for keys rather than by enumerating them. A LIST here would be priced like a
    // PUT, return at most 1000 keys, and be inherently serial.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(4);
    let v = Arc::new(s.as_tenant(t));
    {
        let dead = Engine::new(Arc::clone(&v), t, LaneId(5));
        for i in 0..4 {
            dead.write("idx", vec![doc(&format!("d{i}"), i)])
                .await
                .unwrap();
            dead.flush().await.unwrap();
        }
    }
    let before_list = s.count(t, OpClass::List);
    let tail = lanes::tail(&*v, t, LaneId(5), 0).await.unwrap();
    assert_eq!(tail, 4, "four flushes means the tail is at 4");
    assert_eq!(
        s.count(t, OpClass::List) - before_list,
        0,
        "tail discovery must not LIST"
    );

    // An untouched lane has an empty tail rather than an error.
    assert_eq!(lanes::tail(&*v, t, LaneId(99), 0).await.unwrap(), 0);
}

#[tokio::test]
async fn probing_resumes_from_a_watermark_rather_than_from_zero() {
    // After a fold, everything below the watermark is already in a segment. The saving is
    // not a few reads on a short lane -- it is that a lane with thousands of folded
    // bundles is never re-probed, which is the difference between recovery costing O(1)
    // and O(everything ever written).
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(5);
    let v = Arc::new(s.as_tenant(t));
    let e = Engine::new(Arc::clone(&v), t, LaneId(1));
    for i in 0..300 {
        e.write("idx", vec![doc(&format!("d{i}"), i)])
            .await
            .unwrap();
        e.flush().await.unwrap();
    }

    let before = s.count(t, OpClass::Read);
    assert_eq!(lanes::tail(&*v, t, LaneId(1), 0).await.unwrap(), 300);
    let from_zero = s.count(t, OpClass::Read) - before;

    let before = s.count(t, OpClass::Read);
    assert_eq!(lanes::tail(&*v, t, LaneId(1), 296).await.unwrap(), 300);
    let from_watermark = s.count(t, OpClass::Read) - before;

    assert!(
        from_watermark * 4 < from_zero,
        "resuming from a watermark cost {from_watermark} reads against {from_zero} from zero"
    );
}

#[tokio::test]
async fn a_probe_window_grows_so_a_long_lane_costs_few_round_trips() {
    // Width, not depth. A flat window would make a long lane linear in ROUND TRIPS, which
    // is the one cost that cannot be parallelised away.
    use pstore_testkit::depth::DepthCounting;
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let t = TenantId(6);
    let e = Engine::new(Arc::clone(&s), t, LaneId(1));
    for i in 0..500 {
        e.write("idx", vec![doc(&format!("d{i}"), i)])
            .await
            .unwrap();
        e.flush().await.unwrap();
    }
    s.reset();
    assert_eq!(lanes::tail(&*s, t, LaneId(1), 0).await.unwrap(), 500);
    assert!(
        s.depth() <= 8,
        "500 bundles took {} rounds to find",
        s.depth()
    );
}
