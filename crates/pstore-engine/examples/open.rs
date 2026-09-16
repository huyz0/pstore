//! **M6i.2** — what an open costs, against the two counts the exit criterion conflates.
//!
//! `roadmap.md`'s M6 exit reads *"1M indexes, open latency unaffected by index count, zero
//! LISTs on any hot path"*. There are two index counts in that sentence and they are not the
//! same claim.
//!
//! ⚠️ **Across tenants** the claim is structural: every key an open touches is derived from
//! the tenant id, so a deployment's size cannot reach the read path. That is arm 2, and a
//! measurement is what turns "cannot" from an argument into a number.
//!
//! ⚠️ **Within one tenant** it is not obviously true and nothing had measured it.
//! `Head.indexes` is a `BTreeMap<String, Vec<SegmentRef>>` living in one object that
//! `head::read` fetches **whole**, so every index a tenant owns is bytes on every open of
//! every *other* index it owns. That is arm 1, and the number it reports is the knee.
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

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_engine::{Engine, Head, SegmentRef};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

/// The tenant every request is billed to, so one counter sees the whole harness.
const BILL: TenantId = TenantId(0);
const DIM: usize = 8;
/// The index actually scanned. Named so it sorts first and stays findable among the rest.
const REAL: &str = "index-0000000";

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

fn doc(i: usize) -> Document {
    let mut st = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        (st >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    Document::new(format!("d{i:05}"), (0..DIM).map(|_| next()).collect())
}

/// A tenant with one real, scannable index and `k - 1` further index entries beside it.
///
/// ⚠️ The extra entries are **synthetic**, and they point at the real segment rather than at
/// keys with nothing behind them. Folding `k` real indexes would measure the writer; what is
/// under test is the read, and a HEAD with `k` entries is the state a reader meets either way.
async fn tenant_with_indexes<S: BlobStore>(store: &Arc<S>, t: TenantId, k: usize) -> Engine<S> {
    let e = Engine::new(Arc::clone(store), t, LaneId(1));
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
        // `commit_head_for_test` is the only door onto an arbitrary HEAD. A harness measuring
        // the shape of HEAD is exactly the caller its doc contemplates: the state is reachable
        // in a deployment, and building it through `fold` would measure something else.
        e.commit_head_for_test(|h| {
            for i in 1..k {
                h.indexes.insert(format!("index-{i:07}"), refs.clone());
            }
        })
        .await
        .unwrap();
    }
    e
}

/// One open, as the requests and the bytes it costs.
async fn open_cost<S: BlobStore, T: BlobStore>(
    acct: &Accounted<S>,
    e: &Engine<T>,
) -> (u64, u64, usize) {
    let before = (
        acct.count(BILL, OpClass::Read),
        acct.bytes(BILL, OpClass::Read),
    );
    let rows = e.scan(REAL, None).await.unwrap().len();
    (
        acct.count(BILL, OpClass::Read) - before.0,
        acct.bytes(BILL, OpClass::Read) - before.1,
        rows,
    )
}

/// ⚠️ **Arm 1 — a tenant's own index count.** The one the exit criterion does not distinguish.
async fn by_index_count() {
    println!("## open cost against the tenant's own index count");
    println!();
    println!("| indexes | reads | read bytes | HEAD bytes | bytes/index |");
    println!("|---|---|---|---|---|");
    for k in [1usize, 50, 500, 5_000] {
        let acct = Arc::new(Accounted::new(MemoryStore::new()));
        let store = Arc::new(acct.as_tenant(BILL));
        let e = tenant_with_indexes(&store, TenantId(7), k).await;
        let head_bytes = e.head_for_test().await.encode().len();
        let (reads, bytes, rows) = open_cost(&acct, &e).await;
        assert_eq!(
            rows, 64,
            "the scan stopped returning the documents it opened"
        );
        assert_eq!(
            acct.count(BILL, OpClass::List),
            0,
            "an open listed -- zero LIST is the claim, not a target"
        );
        println!(
            "| {k} | {reads} | {bytes} | {head_bytes} | {:.1} |",
            head_bytes as f64 / k as f64
        );
    }
    println!();
}

/// ⚠️ **Arm 2 — the deployment's tenant count.** Keys are derived, so the claim is that this
/// column is constant. Filler tenants are written as HEAD objects at their own derived keys:
/// what makes an open cheap or dear is what the store holds, not how it came to hold it.
async fn by_deployment_size(sizes: &[u32]) {
    println!("## open cost against the deployment's size");
    println!();
    println!("| tenants in store | reads | read bytes | rss |");
    println!("|---|---|---|---|");
    for &n in sizes {
        let acct = Arc::new(Accounted::new(MemoryStore::new()));
        let store = Arc::new(acct.as_tenant(BILL));
        let e = tenant_with_indexes(&store, TenantId(7), 50).await;
        // ⚠️ **A distinct payload per tenant, deliberately.** One shared `Bytes` cloned a
        // million times is refcounted, so the store would hold 1M keys over a single 5 KB
        // buffer and the run would prove only that a hash map can hold a million keys. Each
        // filler HEAD names its own tenant, so the store really does hold 1M distinct objects
        // of ~5 KB -- which is the deployment the claim is about.
        for i in 0..n {
            // Tenant 7 is the one being opened; overwriting its HEAD would measure the filler.
            if u128::from(i) == 7 {
                continue;
            }
            let filler = Head {
                epoch: pstore_types::Epoch(1),
                nonce: u64::from(i),
                indexes: (0..50)
                    .map(|j| {
                        (
                            format!("t{i:08}-index-{j:07}"),
                            vec![SegmentRef {
                                key: format!("t/{i:08}/seg/{j:08}"),
                                rows: 64,
                            }],
                        )
                    })
                    .collect(),
                watermarks: Default::default(),
                graveyard: Default::default(),
                // The filler measures HEAD's SIZE, and a schema is part of it now: a tenant
                // with 50 indexes carries 50 schema entries, so leaving them out would
                // measure a HEAD no deployment has.
                schemas: (0..50)
                    .map(|j| {
                        (
                            format!("t{i:08}-index-{j:07}"),
                            pstore_engine::IndexSchema {
                                dims: 8,
                                text_field: String::new(),
                            },
                        )
                    })
                    .collect(),
                schema_rejects: Default::default(),
            };
            store
                .put(
                    &Head::key(TenantId(u128::from(i))),
                    bytes::Bytes::from(filler.encode()),
                )
                .await
                .unwrap();
        }
        let (reads, read_bytes, rows) = open_cost(&acct, &e).await;
        assert_eq!(rows, 64);
        assert_eq!(acct.count(BILL, OpClass::List), 0, "an open listed");
        println!("| {n} | {reads} | {read_bytes} | {:.2} GB |", rss_gb());
    }
    println!();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("# Open cost at scale. ⚠️ provisional: WSL2, a MemoryStore, one run.");
    println!();
    println!("Request counts and byte counts are exact; wall clock is not measured here.");
    println!();
    by_index_count().await;
    // ⚠️ 1M filler HEADs of 50 indexes each is a few gigabytes resident. Opt out with
    // PSTORE_OPEN_SMALL rather than by editing the number, so the ledger and the default agree.
    let sizes: &[u32] = if std::env::var("PSTORE_OPEN_SMALL").is_ok() {
        &[1, 10_000]
    } else {
        &[1, 10_000, 1_000_000]
    };
    by_deployment_size(sizes).await;
}
