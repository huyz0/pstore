//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M160: an index's durable operations that no fold has committed, counted across every
//! lane from the bundles' own directories, at a constant depth.

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass, TenantView};
use pstore_engine::{Engine, PROGRESS_BUNDLES, bundle_key};
use pstore_format::Document;
use pstore_types::{LaneId, Seq, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(160);

type E = Engine<TenantView<MemoryStore>>;

struct World {
    acct: Accounted<MemoryStore>,
}

impl World {
    fn new() -> Self {
        Self {
            acct: Accounted::new(MemoryStore::new()),
        }
    }
    fn engine(&self, lane: u64) -> E {
        Engine::new(Arc::new(self.acct.as_tenant(T)), T, LaneId(lane))
    }
    fn count(&self, class: OpClass) -> u64 {
        self.acct.count(T, class)
    }
}

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

/// One bundle on `e`'s lane holding `ids` for `index`.
async fn bundle(e: &E, index: &str, ids: &[&str]) {
    e.write(index, ids.iter().map(|id| doc(id)).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
}

/// `index`'s operation count and byte length in one bundle, read from its footer and
/// directory by hand: the layout the engine's reader must agree with.
async fn entry(w: &World, lane: u64, seq: u64, index: &str) -> Option<(u64, u64)> {
    let body = w
        .acct
        .as_tenant(T)
        .get(&bundle_key(T, LaneId(lane), Seq(seq)))
        .await
        .unwrap();
    let foot = &body[body.len() - 28..];
    let at = u64::from_le_bytes(foot[8..16].try_into().unwrap()) as usize;
    let n = u32::from_le_bytes(foot[16..20].try_into().unwrap()) as usize;
    let dir = &body[at..at + n];
    let mut i = 4;
    for _ in 0..u32::from_le_bytes(dir[..4].try_into().unwrap()) {
        let nlen = u32::from_le_bytes(dir[i..i + 4].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&dir[i + 4..i + 4 + nlen]).unwrap();
        i += 4 + nlen + 8;
        let len = u32::from_le_bytes(dir[i..i + 4].try_into().unwrap());
        let docs = u32::from_le_bytes(dir[i + 4..i + 8].try_into().unwrap());
        i += 8;
        if name == index {
            return Some((u64::from(docs), u64::from(len)));
        }
    }
    None
}

#[tokio::test]
async fn progress_counts_other_lanes() {
    let w = World::new();
    let (a, b) = (w.engine(1), w.engine(2));
    bundle(&a, "idx", &["a"]).await;
    let folded = a.fold().await.unwrap();
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.bytes, p.exact, p.held), (0, 0, true, true));
    assert_eq!(p.epoch, folded);
    // Another process's durable writes: two bundles, one carrying another index too, and a
    // delete; and an index no fold has made yet.
    b.write("idx", vec![doc("x"), doc("y")]).await.unwrap();
    b.write("other", vec![doc("o")]).await.unwrap();
    b.flush().await.unwrap();
    b.delete("idx", vec!["a".into()]).await.unwrap();
    b.write("fresh", vec![doc("f")]).await.unwrap();
    b.flush().await.unwrap();
    let (r0, b0) = entry(&w, 2, 0, "idx").await.unwrap();
    let (r1, b1) = entry(&w, 2, 1, "idx").await.unwrap();
    assert_eq!((r0, r1), (2, 1));
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.bytes, p.exact, p.held), (3, b0 + b1, true, true));
    let fresh = a.progress("fresh").await.unwrap();
    let (rf, bf) = entry(&w, 2, 1, "fresh").await.unwrap();
    assert_eq!((fresh.rows, fresh.bytes, fresh.held), (rf, bf, false));
    assert_eq!(a.progress("absent").await.unwrap().rows, 0);
    // Folded, by anyone: nothing left, as of that fold.
    let folded = a.fold().await.unwrap();
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.bytes, p.exact, p.epoch), (0, 0, true, folded));
}

#[tokio::test]
async fn progress_is_bounded() {
    let w = World::new();
    let (a, b, c) = (w.engine(1), w.engine(2), w.engine(3));
    for i in 0..=PROGRESS_BUNDLES {
        bundle(&b, "idx", &[&format!("r{i}")]).await;
    }
    // One lane holds 65: its watermark and the 63 after it are read.
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.exact), (64, false));
    // Two present lanes share the budget: 1 + 31 of `b`'s, and `c`'s one.
    bundle(&c, "idx", &["c"]).await;
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.exact), (33, false));
    a.fold().await.unwrap();
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.exact), (0, true));
    // Under the budget, every bundle is read: 31 a lane over two lanes, each lane's last
    // probe answering absent. A lane whose every probe answered present may hold more.
    for i in 0..31 {
        bundle(&b, "idx", &[&format!("s{i}")]).await;
        bundle(&c, "idx", &[&format!("t{i}")]).await;
    }
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.exact), (62, true));
    bundle(&c, "idx", &["t31"]).await;
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.exact), (63, false));
}

#[tokio::test]
async fn progress_costs() {
    let w = World::new();
    let engines: Vec<E> = (1..=3).map(|l| w.engine(l)).collect();
    for e in &engines {
        bundle(e, "idx", &["a"]).await;
    }
    engines[0].fold().await.unwrap();
    let reader = w.engine(9);
    let (r, wr, l) = (
        w.count(OpClass::Read),
        w.count(OpClass::Write),
        w.count(OpClass::List),
    );
    // Idle: HEAD, the registry, and one probe for each of three lanes.
    assert_eq!(reader.progress("idx").await.unwrap().rows, 0);
    assert_eq!(w.count(OpClass::Read) - r, 2 + 3);
    // Two bundles on one lane: its 63 forward probes, and a tail read for each bundle.
    bundle(&engines[1], "idx", &["b"]).await;
    bundle(&engines[1], "idx", &["c"]).await;
    let r = w.count(OpClass::Read);
    let (wr2, l2) = (w.count(OpClass::Write), w.count(OpClass::List));
    assert_eq!(reader.progress("idx").await.unwrap().rows, 2);
    assert_eq!(w.count(OpClass::Read) - r, 2 + 3 + 63 + 2);
    assert_eq!(w.count(OpClass::Write), wr2, "progress wrote");
    assert_eq!((w.count(OpClass::List), l), (l2, l2), "progress listed");
    assert!(wr <= wr2);
}

#[tokio::test]
async fn progress_reads_the_lowest_lanes_past_the_budget() {
    let w = World::new();
    let engines: Vec<E> = (1..=PROGRESS_BUNDLES as u64 + 1)
        .map(|l| w.engine(l))
        .collect();
    for e in &engines {
        bundle(e, "idx", &["a"]).await;
    }
    // 65 present lanes: 64 are read, none probed past its watermark.
    let p = w.engine(1000).progress("idx").await.unwrap();
    assert_eq!((p.rows, p.exact), (64, false));
}

#[tokio::test]
async fn progress_stops_a_lane_at_its_first_gap() {
    let w = World::new();
    let (a, b) = (w.engine(1), w.engine(2));
    for i in 0..4 {
        bundle(&b, "idx", &[&format!("r{i}")]).await;
    }
    assert_eq!(a.progress("idx").await.unwrap().rows, 4);
    // A lane is dense, so nothing past a missing key is counted.
    w.acct
        .as_tenant(T)
        .delete_batch(&[bundle_key(T, LaneId(2), Seq(2))])
        .await
        .unwrap();
    let p = a.progress("idx").await.unwrap();
    assert_eq!((p.rows, p.exact), (2, true));
}

#[tokio::test]
async fn progress_reads_only_a_bundles_tail() {
    let w = World::new();
    let (a, b) = (w.engine(1), w.engine(2));
    let ids: Vec<String> = (0..2000).map(|i| format!("row-{i:05}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    bundle(&b, "idx", &refs).await;
    let key = bundle_key(T, LaneId(2), Seq(0));
    let len = w.acct.as_tenant(T).head(&key).await.unwrap();
    assert!(
        len > 4 * 4096,
        "the bundle is {len} bytes, not much past a tail"
    );
    w.acct.record_ranges();
    assert_eq!(a.progress("idx").await.unwrap().rows, 2000);
    let read: u64 = w
        .acct
        .ranges()
        .iter()
        .filter(|(k, _)| *k == key)
        .map(|(_, r)| r.end - r.start)
        .sum();
    assert_eq!(read, 4096, "one 4 KiB tail of a {len}-byte bundle");
}
