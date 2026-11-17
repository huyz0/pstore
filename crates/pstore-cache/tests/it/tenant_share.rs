//! M280: a per-tenant ceiling on each class's memory quota, measured against one shared LRU.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{BlobStore, Class, Key, MemoryStore};
use pstore_cache::{CacheCore, Caching};
use std::sync::Arc;

/// One cached entry: a 1 KiB range.
const ENTRY: u64 = 1024;
/// The bulk quota: 64 entries.
const QUOTA: usize = 64 * ENTRY as usize;
/// The share the measurement compares: a quarter of the quota per tenant.
const SHARE: u16 = 250;

fn key(tenant: u128) -> Key {
    Key::new(format!("0000/tnt/{tenant}/idx/docs/seg/L0/one.seg"))
}

/// A store holding one 4 MiB object per tenant, ranges spaced past its coalescing gap.
async fn store(tenants: u128) -> Arc<MemoryStore> {
    let s = Arc::new(MemoryStore::with_coalesce_gap(16));
    for t in 1..=tenants {
        s.put(&key(t), Bytes::from(vec![7u8; 4 << 20]))
            .await
            .unwrap();
    }
    s
}

async fn read(view: &Caching<MemoryStore>, tenant: u128, entry: u64) {
    let at = entry * ENTRY * 2;
    view.get_range_as(&key(tenant), at..at + ENTRY, Class::Bulk)
        .await
        .unwrap();
}

/// What fraction of each tenant's bytes were hits, in thousandths, after `rounds` rounds in
/// which tenant `t` reads entries `0..working(t)` in order (a working set re-read), and the
/// scanner, if any, reads `scan` new entries.
async fn run(
    share: Option<u16>,
    tenants: u128,
    working: impl Fn(u128) -> u64,
    scan: u64,
) -> Vec<u64> {
    let inner = store(tenants + 1).await;
    let core = Arc::new(CacheCore::with_quotas(0, 0, QUOTA).with_tenant_share(share));
    let views: Vec<Caching<MemoryStore>> = (0..=tenants)
        .map(|_| Caching::over(Arc::clone(&inner), Some(Arc::clone(&core))))
        .collect();
    let scanner = tenants + 1;
    let mut next = 0;
    let mut base = vec![pstore_cache::Traffic::default(); views.len()];
    for round in 0..20 {
        for t in 1..=tenants {
            for e in 0..working(t) {
                read(&views[t as usize - 1], t, e).await;
            }
        }
        // The cold scan: never the same entry twice.
        for _ in 0..scan {
            read(&views[tenants as usize], scanner, next).await;
            next += 1;
        }
        // The first round fills the cache: only later rounds are measured.
        if round == 0 {
            base = views.iter().map(Caching::traffic).collect();
        }
    }
    (1..=tenants)
        .map(|t| {
            let (now, then) = (views[t as usize - 1].traffic(), base[t as usize - 1]);
            let hit = now.hit_bytes - then.hit_bytes;
            let miss = now.miss_bytes - then.miss_bytes;
            hit * 1000 / (hit + miss).max(1)
        })
        .collect()
}

/// A workload: its name, its tenants, each tenant's working set, and the scan a round.
type Workload = (&'static str, u128, fn(u128) -> u64, u64);

#[tokio::test]
async fn a_tenant_share_measured_against_one_shared_lru() {
    // ⚠️ The numbers M280's default rests on. Deterministic: one task, a fixed order.
    let workloads: [Workload; 4] = [
        // A cold scan of 64 entries a round beside 4 small tenants of 8 entries each.
        ("cold scan beside 4 small tenants", 4, |_| 8, 64),
        // One tenant whose working set is half the quota.
        ("one tenant, working set 32 entries", 1, |_| 32, 0),
        // 8 small tenants, 6 entries each: 48 of 64.
        ("8 small tenants", 8, |_| 6, 0),
        // Two tenants of 24 entries each: 48 of 64.
        ("2 tenants of 24 entries", 2, |_| 24, 0),
    ];
    let mut rows = Vec::new();
    for (name, tenants, working, scan) in workloads {
        let shared = run(None, tenants, working, scan).await;
        let capped = run(Some(SHARE), tenants, working, scan).await;
        println!("{name}: shared LRU hit permille {shared:?}, share {SHARE} {capped:?}");
        rows.push((shared, capped));
    }
    // The scan walks every small tenant out of one shared LRU; capped, it evicts itself.
    assert_eq!(rows[0].0, [0, 0, 0, 0]);
    assert_eq!(rows[0].1, [1000, 1000, 1000, 1000]);
    // ⚠️ The downside: a tenant whose working set is above its share thrashes, alone on the
    // node, where one shared LRU served all of it.
    assert_eq!(rows[1].0, [1000]);
    assert_eq!(rows[1].1, [0]);
    // Working sets inside the share: no difference.
    assert_eq!(rows[2].0, rows[2].1);
    // ⚠️ Two tenants above their share and inside the quota: the share thrashes both.
    assert_eq!(rows[3].0, [1000, 1000]);
    assert_eq!(rows[3].1, [0, 0]);
}

#[tokio::test]
async fn no_share_is_one_shared_lru_and_a_share_of_1000_is_none() {
    for share in [None, Some(1000), Some(4000)] {
        let core = CacheCore::with_quotas(0, 0, QUOTA).with_tenant_share(share);
        assert_eq!(core.tenant_share(), None, "{share:?}");
    }
    assert_eq!(
        CacheCore::with_quotas(0, 0, QUOTA)
            .with_tenant_share(Some(SHARE))
            .tenant_share(),
        Some(SHARE)
    );
}

#[tokio::test]
async fn a_key_of_no_tenant_is_never_capped() {
    // A quarter-share core; 32 entries of a key outside the tenant layout stay resident.
    let inner = Arc::new(MemoryStore::with_coalesce_gap(16));
    let k = Key::new("conformance/1/blob");
    inner
        .put(&k, Bytes::from(vec![1u8; 1 << 20]))
        .await
        .unwrap();
    let core = Arc::new(CacheCore::with_quotas(0, 0, QUOTA).with_tenant_share(Some(SHARE)));
    let view = Caching::over(Arc::clone(&inner), Some(core));
    for e in 0..32u64 {
        let at = e * ENTRY * 2;
        view.get_range_as(&k, at..at + ENTRY, Class::Bulk)
            .await
            .unwrap();
    }
    assert_eq!(view.resident_in(Class::Bulk), 32 * ENTRY as usize);
}
