//! **M7b.4** — backlog row 19: is the HEAD byte cost per index worth a layout change?
//!
//! [`open.rs`](open.rs) measured the cost (M6i): an open reads HEAD whole, so a tenant pays
//! ~106 bytes per index it owns on every open of every *other* index. The backlog row asks the
//! next question and names the shape to ask it at: *"a spec that argues the trade with the
//! ~50-index shape a real tenant has, not the 5,000 that makes the number dramatic."*
//!
//! The trade is **bytes against a round trip**. Every way of not reading the whole manifest —
//! a HEAD sharded per index, an offset table read before the entry — costs one more sequential
//! fetch, and `02-object-storage/cost-and-latency.md` §2 is unambiguous about the exchange
//! rate: *"the round trip is the unit of cost, not the byte"*, ~30 ms a hop, and in-region
//! egress is free, so the surplus bytes cost **time and nothing else**.
//!
//! So this prints the crossover: the index count at which the surplus HEAD bytes take as long
//! to transfer as the round trip that removing them would add. Both constants are printed with
//! the answer, because the answer is only as good as they are and neither is measurable here.
//!
//!   cargo run -p pstore-engine --example head_cost
#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::{Engine, SegmentRef};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const BILL: TenantId = TenantId(0);
const REAL: &str = "index-0000000";

/// ⚠️ **Stated, not measured, and that is why it is printed.** `cost-and-latency.md` §2:
/// S3 Standard small-object GET is ~15–60 ms p50. 30 ms is the figure the three-round-trip
/// budget itself is derived from, so using anything else here would argue against a budget
/// with a number the budget does not use.
const RTT_MS: f64 = 30.0;
/// Per-stream throughput for one ranged GET. ⚠️ **The generous end on purpose**: a *higher*
/// figure makes bytes cheaper and pushes the crossover further out, so the conclusion this
/// harness reaches is the one that survives the assumption most favourable to *not* changing
/// the layout being wrong. OQ-3 and M0b are what replace it with a measurement.
const MB_PER_S: f64 = 100.0;

fn doc(i: usize) -> Document {
    Document::new(format!("d{i:05}"), vec![i as f32, 0.5, -0.25, 1.0])
}

/// One open of `REAL` on a tenant owning `k` indexes: (reads, read bytes, HEAD bytes).
async fn open_at(k: usize) -> (u64, u64, usize) {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(BILL));
    let e = Engine::new(Arc::clone(&store), TenantId(7), LaneId(1));
    e.write(REAL, (0..64).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    if k > 1 {
        let refs: Vec<SegmentRef> = e
            .head_for_test()
            .await
            .indexes
            .remove(REAL)
            .expect("the real index was just folded");
        e.commit_head_for_test(|h| {
            for i in 1..k {
                h.indexes.insert(format!("index-{i:07}"), refs.clone());
            }
        })
        .await
        .unwrap();
    }
    let head_bytes = e.head_for_test().await.encode().len();
    let before = (
        acct.count(BILL, OpClass::Read),
        acct.bytes(BILL, OpClass::Read),
    );
    assert_eq!(e.scan(REAL, None).await.unwrap().len(), 64);
    assert_eq!(acct.count(BILL, OpClass::List), 0, "an open listed");
    (
        acct.count(BILL, OpClass::Read) - before.0,
        acct.bytes(BILL, OpClass::Read) - before.1,
        head_bytes,
    )
}

#[tokio::main]
async fn main() {
    println!("# Row 19 — the HEAD byte cost per index, and what it is worth\n");
    println!("⚠️ `provisional`: WSL2, a `MemoryStore`, one run. Relative only (D-104).\n");
    println!("| indexes | reads | read bytes | surplus over K=1 | surplus ms @ {MB_PER_S} MB/s |");
    println!("|---|---|---|---|---|");

    let (_, base_bytes, _) = open_at(1).await;
    let mut per_index = 0.0;
    for k in [1usize, 10, 50, 100, 500, 5_000] {
        let (reads, bytes, _head) = open_at(k).await;
        assert_eq!(reads, 3, "the open stopped costing 3 reads at k={k}");
        let surplus = bytes.saturating_sub(base_bytes);
        let ms = surplus as f64 / (MB_PER_S * 1_048.576);
        if k == 50 {
            per_index = surplus as f64 / 49.0;
        }
        println!("| {k} | {reads} | {bytes} | {surplus} | {ms:.3} |");
    }

    // The crossover: surplus_bytes(K) / throughput == RTT.
    let rtt_bytes = RTT_MS * MB_PER_S * 1_048.576;
    let crossover = rtt_bytes / per_index;
    println!(
        "\n- Measured at the real shape: **{per_index:.1} bytes per index** at K=50.\n\
         - One round trip at {RTT_MS} ms is worth **{rtt_bytes:.0} bytes** at {MB_PER_S} MB/s.\n\
         - **Crossover: K ≈ {crossover:.0} indexes.** Below it, reading the whole manifest is \
         cheaper than the extra fetch that avoiding it costs.\n\
         - The product ceiling is **50 indexes per tenant** (`README.md`), which is \
         **{:.0}×** below the crossover.",
        crossover / 50.0
    );
    println!(
        "\n⚠️ Reads are 3 at every K above, so this is a byte cost and not a depth cost. The \
         budget being traded away is the one that is flat: D-34 allows three sequential round \
         trips and an open already uses all three."
    );
}
