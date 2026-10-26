//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What an open pays for the indexes it is not opening — M6i.
//!
//! ⚠️ **Written because a measurement found it, not because a bug did.** M6i measured one open
//! at 1, 50, 500 and 5,000 indexes and the reads stayed at 3 while the *bytes* went
//! 7,903 → 13,097 → 60,797 → 537,797: about **106 bytes of HEAD per index the tenant owns**,
//! paid on every open of every other index. At 5,000 indexes 98.6% of an open is HEAD.
//!
//! That is not a defect — `roadmap.md`'s M6 exit says *"open latency unaffected by index
//! count"*, and across **tenants** it is, because keys are derived. Within one tenant it is
//! not, and the relationship had never been written down anywhere a change would have to face
//! it. This pins the number so a later HEAD layout argues with a measurement instead of
//! rediscovering one.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::{Engine, SegmentRef};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const BILL: TenantId = TenantId(0);
const REAL: &str = "index-0000000";

fn doc(i: usize) -> Document {
    Document::new(format!("d{i:05}"), vec![i as f32, 0.5, -0.25, 1.0])
}

/// One open of `REAL` on a tenant owning `k` indexes: (bytes read, encoded HEAD length).
async fn open_bytes(k: usize) -> (u64, usize) {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(BILL));
    let e = Engine::new(Arc::clone(&store), TenantId(7), LaneId(1));
    e.write(REAL, (0..8).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    if k > 1 {
        let refs: Vec<SegmentRef> = e.head_for_test().await.indexes[REAL].clone();
        e.commit_head_for_test(|h| {
            for i in 1..k {
                h.indexes.insert(format!("index-{i:07}"), refs.clone());
            }
        })
        .await
        .unwrap();
    }
    let head_len = e.head_for_test().await.encode().len();
    let before = acct.bytes(BILL, OpClass::Read);
    assert_eq!(e.scan(REAL, None).await.unwrap().len(), 8);
    assert_eq!(acct.count(BILL, OpClass::List), 0, "an open listed");
    (acct.bytes(BILL, OpClass::Read) - before, head_len)
}

#[tokio::test]
async fn an_open_reads_the_whole_head_including_the_indexes_it_is_not_opening() {
    let (few, few_head) = open_bytes(1).await;
    let (many, many_head) = open_bytes(200).await;

    // ⚠️ The **exact** difference, not merely "more". `>=` passes for a HEAD read as a fixed
    // prefix, for a HEAD read twice, and for a segment fetch that happens to grow -- none of
    // which is the claim. Every extra byte an open pays between these two tenants is HEAD, and
    // the segments they scan are identical.
    assert_eq!(
        many - few,
        (many_head - few_head) as u64,
        "an open at 200 indexes read {many} bytes and at 1 read {few}, but the HEAD grew by \
         {} -- the difference is no longer exactly the manifest",
        many_head - few_head
    );
    // And the relationship is per-index rather than a constant: 199 more entries, and the
    // floor is deliberately far under the ~106 bytes measured so this fails on a defect
    // rather than on an encoding that got tighter.
    assert!(
        many - few > 199 * 32,
        "199 extra indexes added only {} bytes to an open",
        many - few
    );
}

#[tokio::test]
async fn an_open_costs_the_same_requests_however_many_indexes_a_tenant_has() {
    // ⚠️ The other half of the exit criterion, and the half that holds: HEAD is one object, so
    // the index count is bytes and never round trips. A HEAD sharded per index would trade
    // this away, and that trade should be visible as a failing test rather than as a slow query.
    async fn reads(k: usize) -> u64 {
        let acct = Arc::new(Accounted::new(MemoryStore::new()));
        let store = Arc::new(acct.as_tenant(BILL));
        let e = Engine::new(Arc::clone(&store), TenantId(7), LaneId(1));
        e.write(REAL, (0..8).map(doc).collect()).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
        if k > 1 {
            let refs: Vec<SegmentRef> = e.head_for_test().await.indexes[REAL].clone();
            e.commit_head_for_test(|h| {
                for i in 1..k {
                    h.indexes.insert(format!("index-{i:07}"), refs.clone());
                }
            })
            .await
            .unwrap();
        }
        let before = acct.count(BILL, OpClass::Read);
        // ⚠️ Asserted here and not left to the sibling test: a `scan` that resolves no segments
        // at all reads once at every k, so both arms of this test agree and it passes green
        // while measuring nothing.
        assert_eq!(e.scan(REAL, None).await.unwrap().len(), 8);
        acct.count(BILL, OpClass::Read) - before
    }
    assert_eq!(reads(1).await, reads(500).await);
}
