//! **M6i.1** — the OQ-72 workload: N tenants x 50 indexes, seeded, folded, enumerated.
//!
//! ⚠️ **This replaces arithmetic with a measurement.** [M6a](../../../docs/milestones/M6a/VERIFIED.md)
//! says it in its own ledger — *"Not measured at 1M. The invariant is measured at 2,000
//! tenants; 1M is arithmetic on it"* — and `roadmap.md`'s M6 exit asks for *"1M indexes ...
//! zero LISTs on any hot path"*.
//!
//! The invariant under test is that a census costs `width` pointer reads + one read per
//! occupied bucket + one closing root read, and **nothing per tenant**. A single request
//! anywhere in the read path that is per-record breaks it and no functional assertion sees
//! the difference — which is why the arms are compared against each other *and* against the
//! closed form, rather than against a number a previous run printed.
//!
//!   scripts/scale.sh
#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_catalog::{
    Appender, DEFAULT_WIDTH, Root, TenantRecord, Width, enumerate, fold, write_root,
};
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

/// The tenant every request is billed to, so one counter sees the whole harness.
const BILL: TenantId = TenantId(0);
/// Indexes per tenant. `tenancy-scale-model.md` §8's shape: 1M tenants x ~50 indexes.
const INDEXES: usize = 50;

fn rss_gb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<f64>()
                .ok()
        })
        .unwrap_or(0.0)
        / 1_048_576.0
}

/// What one deployment size cost.
struct Arm {
    tenants: u128,
    reads: u64,
    census_bytes: u64,
    peak_rss: f64,
}

async fn arm(n: u128, w: Width) -> Arm {
    let names: Vec<String> = (0..INDEXES).map(|k| format!("index-{k:03}")).collect();
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(BILL));
    write_root(
        store.as_ref(),
        Root {
            epoch: Epoch(1),
            width: w,
        },
        None,
    )
    .await
    .unwrap();

    let t0 = std::time::Instant::now();
    let app = Appender::new(Arc::clone(&store), w);
    for i in 0..n {
        app.observe(&TenantRecord::live(TenantId(i), Epoch(1), &names))
            .await
            .unwrap();
    }
    let seed_rss = rss_gb();
    println!(
        "| seed | {n} tenants x {INDEXES} indexes | {:.1}s | {seed_rss:.2} GB |",
        t0.elapsed().as_secs_f64()
    );

    let t1 = std::time::Instant::now();
    for b in w.all() {
        fold(store.as_ref(), b).await.unwrap();
    }
    let fold_rss = rss_gb();
    println!(
        "| fold | {} buckets | {:.1}s | {fold_rss:.2} GB |",
        w.get(),
        t1.elapsed().as_secs_f64()
    );
    // ⚠️ Checked here and not only at the end: a LIST on the *fold* path is invisible to a
    // census that counts its own requests, and the fold path is where one would be reached for.
    assert_eq!(
        acct.count(BILL, OpClass::List),
        0,
        "seeding or folding issued a LIST"
    );

    let before = (
        acct.count(BILL, OpClass::Read),
        acct.bytes(BILL, OpClass::Read),
    );
    let t2 = std::time::Instant::now();
    let out = enumerate(store.as_ref(), w).await.unwrap();
    let reads = acct.count(BILL, OpClass::Read) - before.0;
    let census_bytes = acct.bytes(BILL, OpClass::Read) - before.1;
    let peak_rss = rss_gb();
    println!(
        "| census | {} records, {reads} reads, {census_bytes} bytes | {:.1}s | {peak_rss:.2} GB |",
        out.records.len(),
        t2.elapsed().as_secs_f64()
    );

    assert_eq!(
        out.records.len() as u128,
        n,
        "the census is short -- a bucket was dropped, which reads as a smaller, faster catalog"
    );
    assert_eq!(
        out.marks.len(),
        w.get() as usize,
        "a bucket came up empty, so this arm is not comparable with a fully occupied one"
    );
    assert_eq!(
        acct.count(BILL, OpClass::List),
        0,
        "the census issued a LIST"
    );
    // ⚠️ Against the closed form, not only against the other arm: two arms that are wrong the
    // same way are equal to each other.
    assert_eq!(
        reads,
        u64::from(w.get()) * 2 + 1,
        "a census of a fully occupied catalog is width pointers + width runs + one root"
    );
    Arm {
        tenants: n,
        reads,
        census_bytes,
        peak_rss,
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let w = Width::new(DEFAULT_WIDTH).expect("the default width");
    // ⚠️ Both arms must be **fully occupied** or the comparison is about empty buckets rather
    // than about tenants: below ~180k the census reads fewer runs than there are buckets.
    // Opt down with PSTORE_SCALE_SMALL rather than by editing the numbers.
    let sizes: &[u128] = if std::env::var("PSTORE_SCALE_SMALL").is_ok() {
        &[200_000]
    } else {
        &[200_000, 1_000_000]
    };
    println!("# The OQ-72 workload. ⚠️ provisional: WSL2, a MemoryStore, one run.");
    println!();
    println!("Request counts and byte counts are exact; wall clock is relative only.");
    println!();
    println!("| stage | what | wall | rss |");
    println!("|---|---|---|---|");
    let mut arms = Vec::new();
    for &n in sizes {
        arms.push(arm(n, w).await);
    }
    println!();
    println!("| tenants | indexes | census reads | census bytes | bytes/index | peak rss |");
    println!("|---|---|---|---|---|---|");
    for a in &arms {
        println!(
            "| {} | {} | {} | {} | {:.1} | {:.2} GB |",
            a.tenants,
            a.tenants * INDEXES as u128,
            a.reads,
            a.census_bytes,
            a.census_bytes as f64 / (a.tenants * INDEXES as u128) as f64,
            a.peak_rss
        );
    }
    if let [first, .., last] = arms.as_slice() {
        assert_eq!(
            first.reads, last.reads,
            "the census cost moved with the tenant count"
        );
        println!();
        println!(
            "census reads are {} at {} tenants and at {} -- {}x the tenants, the same requests",
            last.reads,
            first.tenants,
            last.tenants,
            last.tenants / first.tenants
        );
    }
}
