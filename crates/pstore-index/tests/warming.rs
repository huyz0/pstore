//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Shadow warming (D-44), and the post-scale-out dip it exists to remove.
//!
//! ⚠️ **The dip is measured on the FIRST query, and that is not a shortcut.** The cache warms
//! itself on query one — opening the index admits the segment index section and the centroid
//! table, and by the per-class quotas neither is then evicted. So a mean over 200 queries
//! differs between a warmed and a cold node by about two requests and reports a ratio near
//! 1.005×. The dip lives in the first query, so that is where it is measured.

use bytes::Bytes;
use pstore_blob::{Accounted, BlobStore, Class, Key, MemoryStore, OpClass, TenantView};
use pstore_cache::Caching;
use pstore_format::Document;
use pstore_index::cluster::Params;
use pstore_index::vec_index::{self, Query, Rerank, VecIndex};
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;
use std::sync::Arc;

const DIM: usize = 64;
const SEG: &str = "t/idx/seg";
const CEN: &str = "t/idx/centroids";

struct Rng(u64);
impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        ((self.0 >> 33) as f32 / (1u64 << 31) as f32) - 0.5
    }
}

fn corpus(n: usize) -> Vec<Document> {
    let mut rng = Rng(7);
    (0..n)
        .map(|i| {
            let v: Vec<f32> = (0..DIM).map(|_| rng.next_f32()).collect();
            Document::new(format!("d{i}"), v)
        })
        .collect()
}

/// A cache over a counted store, in the layering the criteria require: cache outermost.
fn stack() -> (
    Caching<TenantView<MemoryStore>>,
    Arc<Accounted<MemoryStore>>,
) {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    // Generous quotas: this is about what warming fetches, not about eviction.
    let cache = Caching::with_quotas(
        Arc::new(acct.as_tenant(TenantId(0))),
        1 << 20,
        1 << 20,
        1 << 20,
    );
    (cache, acct)
}

async fn put<S: BlobStore>(store: &S, docs: &[Document]) {
    let built = vec_index::build(
        docs,
        Params {
            target_list_size: 40,
            exact_scan_threshold: 64,
            ..Params::default()
        },
    );
    store
        .put(&Key::new(SEG), built.segment.clone())
        .await
        .unwrap();
    if let Some(c) = &built.centroids {
        store
            .put(&Key::new(CEN), Bytes::from(c.encode()))
            .await
            .unwrap();
    }
}

fn reads(acct: &Accounted<MemoryStore>) -> u64 {
    acct.count(TenantId(0), OpClass::Read)
}

#[tokio::test]
async fn warming_fetches_no_bulk_bytes() {
    // ⚠️ D-44 is a rule about WHAT, not about how much. Classes 1–4 are 0.1–1% of the bytes
    // and unblock every query; a warm-up that also pulls the vector section is a cache fill
    // wearing another name, and it is capped by device endurance at ~67 MB/s.
    let (cache, _) = stack();
    put(&cache, &corpus(600)).await;

    VecIndex::warm(&cache, &Key::new(SEG), &Key::new(CEN))
        .await
        .unwrap();

    assert!(
        cache.resident_in(Class::Meta) > 0,
        "warming admitted no metadata"
    );
    assert!(
        cache.resident_in(Class::Pinned) > 0,
        "warming admitted no centroids"
    );
    assert_eq!(
        cache.resident_in(Class::Bulk),
        0,
        "warming pulled bulk bytes: this is a cache fill, not a warm-up"
    );
}

#[tokio::test]
async fn warming_costs_the_same_at_any_row_count() {
    // ⚠️ O(1) per segment, never one request per document. A warm-up that scales with the
    // corpus is the thing it was supposed to avoid.
    let mut costs = Vec::new();
    for n in [600usize, 3_000] {
        let (cache, acct) = stack();
        put(&cache, &corpus(n)).await;
        let before = reads(&acct);
        VecIndex::warm(&cache, &Key::new(SEG), &Key::new(CEN))
            .await
            .unwrap();
        costs.push(reads(&acct) - before);
    }
    assert_eq!(
        costs[0], costs[1],
        "warming cost {} requests at 600 rows and {} at 5,000: it scales with documents",
        costs[0], costs[1]
    );
    assert!(
        costs[0] <= 3,
        "warming cost {} requests for one segment",
        costs[0]
    );
}

#[tokio::test]
async fn warming_an_already_warm_index_is_free() {
    // A repeated placement change must not refill what is already resident.
    let (cache, acct) = stack();
    put(&cache, &corpus(600)).await;
    VecIndex::warm(&cache, &Key::new(SEG), &Key::new(CEN))
        .await
        .unwrap();

    let before = reads(&acct);
    VecIndex::warm(&cache, &Key::new(SEG), &Key::new(CEN))
        .await
        .unwrap();
    assert_eq!(
        reads(&acct) - before,
        0,
        "warming an already-warm index went back to the store"
    );
}

#[tokio::test]
async fn warming_cuts_the_first_query_cost() {
    // ⚠️ The dip, as a DEPTH reduction. A cold first query must walk {footer, centroids} and
    // only then the posting lists — a data-dependent chain, because it cannot know which
    // lists to read until it has the centroids. Warming moves that chain off the query path,
    // leaving one round.
    //
    // ⚠️ NOT a request-count ratio. Warming removes a fixed 2 metadata requests while the
    // query fans out to `p` probes, so a count ratio is `(2 + p) / p` — a property of the
    // probe count, not of warming. A first version asserted "≥2× fewer requests" and measured
    // 8 against 10. Width is free; depth is not.
    let docs = corpus(600);
    let probe = docs[3].vector();
    let query = Query {
        k: 10,
        p: 8,
        rerank: Rerank::Fast,
        ..Query::default()
    };

    let run = |warm_first: bool| {
        let docs = docs.clone();
        async move {
            let depth = Arc::new(DepthCounting::new(MemoryStore::new()));
            let cache = Caching::with_quotas(Arc::clone(&depth), 1 << 20, 1 << 20, 1 << 20);
            put(&cache, &docs).await;
            if warm_first {
                VecIndex::warm(&cache, &Key::new(SEG), &Key::new(CEN))
                    .await
                    .unwrap();
            }
            depth.reset();
            let idx = VecIndex::open(&cache, &Key::new(SEG), &Key::new(CEN), DIM)
                .await
                .unwrap();
            idx.search(&cache, &Key::new(SEG), probe, query)
                .await
                .unwrap();
            (depth.depth(), depth.requests())
        }
    };

    let (cold_depth, cold_reqs) = run(false).await;
    let (warm_depth, warm_reqs) = run(true).await;

    assert!(
        cold_depth >= 2,
        "a cold first query took {cold_depth} sequential rounds; there is no dip to remove"
    );
    assert_eq!(
        warm_depth, 1,
        "a warmed first query took {warm_depth} sequential rounds against {cold_depth} cold — \
         warming did not move the metadata chain off the query path (requests: {warm_reqs} \
         warmed, {cold_reqs} cold)"
    );
}
