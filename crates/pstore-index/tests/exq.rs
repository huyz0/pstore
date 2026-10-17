//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Extended RaBitQ (M97, OQ-170): `B − 1` extra bits beside the 1-bit code.
//!
//! ⚠️ These must pass before any `--prism` tier number is believed. A broken `exq` reads
//! as "the method loses recall", which is a finding about the method, and it would be
//! wrong.

use pstore_index::exq::ExQuantizer;
use pstore_index::rabitq::Quantizer;

const DIM: usize = 384;

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    fn normal(&mut self) -> f32 {
        (0..6).map(|_| self.unit()).sum()
    }
    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.normal()).collect()
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn normalize(v: &mut [f32]) {
    let n = dot(v, v).sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

fn unit(rng: &mut Rng, dim: usize) -> Vec<f32> {
    let mut v = rng.vector(dim);
    normalize(&mut v);
    v
}

/// A query at a controlled angle to `o`, as `tests/rabitq.rs` builds one.
fn near(o: &[f32], rng: &mut Rng, t: f32) -> Vec<f32> {
    let u = unit(rng, o.len());
    let mut q: Vec<f32> = o.iter().zip(&u).map(|(x, n)| x + t * n).collect();
    normalize(&mut q);
    q
}

/// A vector, its centroid, and a query correlated with the vector.
///
/// ⚠️ Correlated with the **residual**, which is what the unbiasing division acts on. A
/// query unrelated to the residual has `<x, q> ≈ 0`, where a multiplicative error is
/// invisible -- the gap `tests/rabitq.rs` documents for the 1-bit code.
struct Pair {
    v: Vec<f32>,
    centroid: Vec<f32>,
    query: Vec<f32>,
}

fn pairs(n: usize, dim: usize, seed: u64) -> Vec<Pair> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|_| {
            let v = unit(&mut rng, dim);
            let centroid: Vec<f32> = unit(&mut rng, dim).iter().map(|x| x * 0.3).collect();
            let query = near(&v, &mut rng, 0.5);
            Pair { v, centroid, query }
        })
        .collect()
}

#[test]
fn exq_search_beats_a_fixed_scale() {
    // The rescale search is the construction: one scale for every vector is a uniform
    // quantizer, and the per-vector critical-value search is what the paper adds. So the
    // chosen code must be at least as good as every critical value it could have chosen,
    // and better on average than the scale that just spans the vector's range.
    let base = Quantizer::new(100);
    let mut rng = Rng(7);
    for bits in [2u32, 4] {
        let exq = ExQuantizer::new(&base, bits).unwrap();
        let top = ((1u32 << (bits - 1)) - 1) as f32;
        let (mut chosen, mut fixed) = (0.0f64, 0.0f64);
        for _ in 0..500 {
            let v = unit(&mut rng, 100);
            let c = vec![0.0f32; 100];
            let r = exq.rotated_residual(&v, &c).unwrap();
            let code = exq.encode_residual(&v, &c).unwrap();
            let best = code.cosine();
            // Every magnitude 0, the state the search starts from.
            let mut seen = exq.cosine_at(&r, 1e-6);
            for x in &r {
                for j in 1..=top as u32 {
                    let t = j as f32 / x.abs();
                    if !t.is_finite() {
                        continue;
                    }
                    // Either side of the critical value: the search evaluated both states.
                    for s in [t * (1.0 - 1e-6), t * (1.0 + 1e-6)] {
                        let other = exq.cosine_at(&r, s);
                        seen = seen.max(other);
                        assert!(
                            best >= other - 1e-5,
                            "B={bits}: chose cosine {best}, but scale {s} gives {other}"
                        );
                    }
                }
            }
            // And it is one of those states, not merely no worse than them: a one-sided
            // check passes against a `cosine_at` that answers 0 (M97's sweep).
            assert!(
                (best - seen).abs() <= 1e-5,
                "B={bits}: chose cosine {best}, but the best critical value gives {seen}"
            );
            let hi = r.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            chosen += f64::from(best);
            fixed += f64::from(exq.cosine_at(&r, (top + 1.0) / hi * (1.0 - 1e-6)));
        }
        // ⚠️ Strictly greater, as the spec says, and no margin: the scale that spans the
        // vector's range is itself per-vector, and at B=4 the search beats it by 4e-4 on
        // average (0.9951 against 0.9947). The per-critical-value check above is the strong
        // half; a margin of 1e-3 was written first and failed on that number.
        assert!(
            chosen > fixed,
            "B={bits}: mean chosen cosine {:.4} does not beat a fixed scale's {:.4}",
            chosen / 500.0,
            fixed / 500.0
        );
    }
}

#[test]
fn exq_error_falls_with_bits() {
    // More bits must mean less error, or the extra bits are not reaching the estimate.
    let base = Quantizer::new(DIM);
    let b2 = ExQuantizer::new(&base, 2).unwrap();
    let b4 = ExQuantizer::new(&base, 4).unwrap();
    let (mut e1, mut e2, mut e4) = (0.0f64, 0.0f64, 0.0f64);
    for p in pairs(1_000, DIM, 11) {
        let truth = dot(&p.v, &p.query);
        let cd = dot(&p.centroid, &p.query);
        let q = base.prepare(&p.query).unwrap();
        let one = base.encode_residual(&p.v, &p.centroid).unwrap();
        let x2 = b2.encode_residual(&p.v, &p.centroid).unwrap();
        let x4 = b4.encode_residual(&p.v, &p.centroid).unwrap();
        e1 += f64::from((truth - base.estimate_residual(&one, &q, cd)).abs());
        e2 += f64::from((truth - b2.estimate_residual(&one, &x2, &q, cd)).abs());
        e4 += f64::from((truth - b4.estimate_residual(&one, &x4, &q, cd)).abs());
    }
    // ⚠️ Strictly, and by a margin: the published error at B=2 is roughly half the 1-bit
    // code's, and at B=4 about a quarter of B=2's. A tie is the extra bits ignored.
    assert!(
        e4 * 1.5 < e2 && e2 * 1.3 < e1,
        "mean |error| over 1,000 pairs: 1-bit {:.5}, B=2 {:.5}, B=4 {:.5}",
        e1 / 1e3,
        e2 / 1e3,
        e4 / 1e3
    );
}

#[test]
fn exq_record_round_trips_and_refuses_bad_shapes() {
    for dim in [384usize, 100] {
        let base = Quantizer::new(dim);
        let padded = dim.next_power_of_two();
        let mut rng = Rng(dim as u64);
        for bits in [2u32, 4] {
            let exq = ExQuantizer::new(&base, bits).unwrap();
            // ⚠️ Over the PADDED dimension: 384d codes 512 coordinates (`rabitq.rs`).
            let len = (bits as usize - 1) * padded / 8 + 8;
            assert_eq!(exq.record_len(), len, "dim {dim}, B={bits}");
            let v = unit(&mut rng, dim);
            let c: Vec<f32> = unit(&mut rng, dim).iter().map(|x| x * 0.3).collect();
            let code = exq.encode_residual(&v, &c).unwrap();
            let mut wire = Vec::new();
            code.write_to(&mut wire);
            assert_eq!(wire.len(), len, "dim {dim}, B={bits}");
            assert_eq!(exq.read(&wire), Some(code.clone()), "dim {dim}, B={bits}");
            // A record read at the wrong stride decodes plausibly and answers wrongly.
            assert_eq!(exq.read(&wire[..len - 1]), None);
            wire.push(0);
            assert_eq!(exq.read(&wire), None);
        }
        for bits in [0u32, 1, 3, 5, 8] {
            assert!(
                ExQuantizer::new(&base, bits).is_err(),
                "B={bits} must be refused"
            );
        }
        // The refusal names the width it refused, or a misconfigured tier reads as silence.
        let refused = ExQuantizer::new(&base, 3).unwrap_err().to_string();
        assert!(refused.contains("not 3"), "{refused}");
        assert!(
            ExQuantizer::new(&base, 4)
                .unwrap()
                .encode_residual(&[0.0; 3], &[0.0; 3])
                .is_err(),
            "a wrong-length vector must be refused"
        );
    }
}

#[test]
fn exq_residual_estimate_is_unbiased() {
    // The `<ȳ, o>` division is what removes the bias, as `<x̄, x>` does for the 1-bit code.
    // Without it a B=4 estimate is low by about 0.005 (−0.0048 measured, M97), which on
    // queries correlated with the residual is about 80 standard errors.
    let base = Quantizer::new(DIM);
    let b4 = ExQuantizer::new(&base, 4).unwrap();
    let errs: Vec<f64> = pairs(2_000, DIM, 13)
        .iter()
        .map(|p| {
            let q = base.prepare(&p.query).unwrap();
            let one = base.encode_residual(&p.v, &p.centroid).unwrap();
            let x4 = b4.encode_residual(&p.v, &p.centroid).unwrap();
            let est = b4.estimate_residual(&one, &x4, &q, dot(&p.centroid, &p.query));
            f64::from(est - dot(&p.v, &p.query))
        })
        .collect();
    let n = errs.len() as f64;
    let mean = errs.iter().sum::<f64>() / n;
    let var = errs.iter().map(|e| (e - mean) * (e - mean)).sum::<f64>() / (n - 1.0);
    let se = (var / n).sqrt();
    assert!(
        mean.abs() <= 3.0 * se,
        "mean signed error {mean:.6} over 2,000 pairs is more than 3 standard errors \
         ({se:.6}) from zero"
    );
}

#[test]
fn a_vector_on_its_centroid_estimates_the_centroid() {
    // No residual, no direction: the estimate is `<c, q>`, as the 1-bit code's is, and not
    // the NaN a zero norm divided by a zero cosine would give.
    let base = Quantizer::new(8);
    let exq = ExQuantizer::new(&base, 4).unwrap();
    let v = [0.5f32, -0.25, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
    let one = base.encode_residual(&v, &v).unwrap();
    let extra = exq.encode_residual(&v, &v).unwrap();
    assert_eq!(extra.cosine(), 0.0);
    let q = base.prepare(&[1.0; 8]).unwrap();
    assert_eq!(exq.estimate_residual(&one, &extra, &q, 0.75), 0.75);
}
