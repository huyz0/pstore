//! Criterion 1: a bucket is derived from the id, and hashing is what defeats structured ids.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_catalog::{Width, bucket_of};
use pstore_types::TenantId;

/// Widest imbalance a hash may show before this is skew rather than noise.
///
/// ⚠️ Not tight for its own sake. At width 256 over 100,000 ids the mean is 390.6 and σ is
/// 19.75, so a strong hash lands near 1.15× and 1.5× is ~5σ of headroom — the test must not
/// go red on a rehash. It is lethal to truncation all the same, because truncation here is
/// not *slightly* worse: it puts an entire id family in one bucket.
const MAX_IMBALANCE: f64 = 1.5;

fn imbalance(ids: impl Iterator<Item = u128>, width: Width) -> f64 {
    let mut counts = vec![0u32; width.get() as usize];
    let mut n = 0u32;
    for id in ids {
        counts[bucket_of(TenantId(id), width) as usize] += 1;
        n += 1;
    }
    let mean = f64::from(n) / f64::from(width.get());
    f64::from(counts.into_iter().max().unwrap_or(0)) / mean
}

#[test]
fn bucket_is_derived_and_hashing_defeats_structured_ids() {
    let w = Width::new(256).expect("256 buckets");
    let n = 100_000u128;

    // ⚠️ Three families, and the first one alone proves nothing. Tenant ids are a `u128` the
    // caller chooses, so the realistic hazard is *structure* in them -- and `id as u32 % width`
    // is perfectly uniform over `0, 1, 2, …`, which is the family a test reaches for first.
    let sequential = imbalance(0..n, w);
    // A discriminator in the high bits: every id's low 64 bits are zero.
    let high_bits = imbalance((0..n).map(|i| i << 64), w);
    // An allocator with a stride that is a multiple of the width.
    let strided = imbalance((0..n).map(|i| i * 65_536), w);

    for (name, got) in [
        ("sequential", sequential),
        ("high bits only", high_bits),
        ("stride 65536", strided),
    ] {
        assert!(
            got < MAX_IMBALANCE,
            "{name}: busiest bucket is {got:.3}x the mean, over {MAX_IMBALANCE}x"
        );
    }
}

#[test]
fn every_bucket_is_inside_the_width() {
    for width in [1u32, 2, 7, 256, 65_536] {
        let w = Width::new(width).expect("a width");
        for id in 0..1_000u128 {
            assert!(bucket_of(TenantId(id), w) < width);
        }
    }
}
