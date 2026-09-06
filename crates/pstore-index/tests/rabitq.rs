//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! RaBitQ, and the two properties it was chosen for.
//!
//! D-11 picks RaBitQ over PQ for reasons that are both testable, and neither of which is
//! raw accuracy:
//!
//! 1. **It needs no training.** PQ wants k-means over a per-index sample; with millions of
//!    tenants that is codebooks to schedule, store, version and re-run as distributions
//!    drift, and a cold ten-document index has no sample to train on.
//! 2. **It has an error bound.** PQ has none and can reach ~100% relative error on real
//!    data. For a product accepting arbitrary customer embeddings, an unbounded worst case
//!    is not a tradeoff, it is a defect. The bound is a guarantee we intend to state, so it
//!    is measured here rather than cited.

use pstore_index::rabitq::{BOUND_EPSILON, Quantizer};

const DIM: usize = 384;
const SEED: u64 = 0x5EED;

/// A deterministic Gaussian-ish source, so a failure replays exactly.
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
    /// Sum of 6 uniforms: near enough to normal for a quantizer test, and cheap.
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

/// A query at a controlled angle to `o`: the regime the index actually runs in.
///
/// The perturbation is a **unit** direction scaled by `t`, so `<o, q> = 1/sqrt(1 + t²)` and
/// the angle is what the caller asked for. Perturbing by an unnormalized Gaussian instead
/// makes "near" mean whatever the dimension happens to make it -- at 384 dimensions a
/// nominal 0.35 produced a query 98% noise, and a bias test built on it passed against a
/// knowingly broken estimator.
fn near(o: &[f32], rng: &mut Rng, t: f32) -> Vec<f32> {
    let mut u = rng.vector(o.len());
    normalize(&mut u);
    let mut q: Vec<f32> = o.iter().zip(&u).map(|(x, n)| x + t * n).collect();
    normalize(&mut q);
    q
}

fn normalize(v: &mut [f32]) {
    let n = dot(v, v).sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

#[test]
fn two_quantizers_encode_identically() {
    // ⚠️ The no-training property, as a byte comparison. Any per-index state -- a sampled
    // codebook, a fitted rotation, a cached norm -- would show up here as a difference,
    // and would be an operational cost multiplied by the number of tenants.
    let mut rng = Rng(1);
    let a = Quantizer::new(DIM);
    let b = Quantizer::new(DIM);
    for _ in 0..64 {
        let mut v = rng.vector(DIM);
        normalize(&mut v);
        assert_eq!(a.encode(&v).unwrap().bits(), b.encode(&v).unwrap().bits());
    }
}

#[test]
fn encoding_is_stable_across_dimensions() {
    // The rotation is seeded from the dimension and a global constant, never from data.
    // A rotation derived from the vectors it will encode is training by another name.
    for dim in [16usize, 64, 128, 384, 768] {
        let mut rng = Rng(dim as u64);
        let mut v = rng.vector(dim);
        normalize(&mut v);
        let q = Quantizer::new(dim);
        assert_eq!(q.encode(&v).unwrap().bits(), q.encode(&v).unwrap().bits());
        // One bit per PADDED dimension, packed. The padding is not free and the
        // assertion says so rather than rounding it away: 384 dimensions cost 512 bits.
        assert_eq!(
            q.encode(&v).unwrap().bits().len(),
            dim.next_power_of_two().div_ceil(8),
            "dim {dim}"
        );
    }
}

#[test]
fn the_measured_failure_rate_is_below_delta() {
    // ⚠️ The bound is PROBABILISTIC, so it is asserted on the empirical failure rate over
    // many pairs and not per-pair. Asserting per-pair would be either flaky or, once
    // widened until it never trips, vacuous.
    //
    // ⚠️ Swept across query angles, and that is not thoroughness for its own sake. The two
    // ways the estimator can be wrong live at opposite ends:
    //   * ORTHOGONAL queries stress the noise term, whose norm is sqrt(1 - <x,q>²) and is
    //     therefore largest when query and vector are unrelated. Worst case for the bound.
    //   * NEAR queries stress the de-biasing division, whose omission is a ~20%
    //     MULTIPLICATIVE error -- invisible in absolute terms when <x,q> ≈ 0, which is what
    //     two random unit vectors in 512 dimensions give you.
    // Built on random pairs alone this test passes with the de-biasing division deleted,
    // which is how that gap was found.
    let q = Quantizer::new(DIM);
    let mut rng = Rng(SEED);
    let mut breaches = 0usize;
    let mut total = 0usize;
    // Sized for the suite, not for the claim: at δ = 1e-3 this tolerates ≤5 breaches. The
    // large sweep belongs in the recall harness (M3.10), which runs outside `cargo test`
    // and so outside `cargo mutants` re-running it once per mutant.
    for t in [f32::INFINITY, 4.0, 1.0, 0.5, 0.25] {
        for _ in 0..1_000 {
            let mut o = rng.vector(DIM);
            normalize(&mut o);
            let query = if t.is_infinite() {
                let mut v = rng.vector(DIM);
                normalize(&mut v);
                v
            } else {
                near(&o, &mut rng, t)
            };
            let code = q.encode(&o).unwrap();
            let est = q.estimate(&code, &query).unwrap();
            total += 1;
            if (est - dot(&o, &query)).abs() > q.error_bound(&code) {
                breaches += 1;
            }
        }
    }
    let rate = breaches as f64 / total as f64;
    assert!(
        rate <= 1e-3,
        "the error bound was breached {breaches} times in {total} pairs (rate {rate:.5}, \
         delta 1e-3, epsilon {BOUND_EPSILON}); either the estimator is wrong or RaBitQ's \
         published guarantee does not hold as implemented"
    );
}

#[test]
fn the_rotation_is_randomised_not_merely_orthogonal() {
    // ⚠️ A plain Walsh-Hadamard transform is orthogonal, so every bound above still holds
    // and every test above still passes without the random sign flips. What it does not do
    // is protect against STRUCTURED input -- and real embeddings are structured, which is
    // the entire reason the transform is randomised rather than fixed.
    //
    // A Hadamard basis row is the adversarial case: an unrandomised transform maps it onto
    // a single axis, so all but one coordinate is zero, the sign pattern carries almost no
    // information, and the code's alignment collapses from ~0.8 to ~1/sqrt(D). The bound
    // stays technically true and becomes so wide it says nothing.
    let q = Quantizer::new(DIM);
    let padded = DIM.next_power_of_two();
    let mut adversarial = vec![0.0f32; DIM];
    for (i, x) in adversarial.iter_mut().enumerate() {
        // The second Hadamard row: alternating signs.
        *x = if i % 2 == 0 { 1.0 } else { -1.0 };
    }
    normalize(&mut adversarial);
    let alignment = q.encode(&adversarial).unwrap().alignment();
    assert!(
        alignment > 0.5,
        "a Hadamard-structured vector encoded with alignment {alignment:.4}; without random \
         sign flips it collapses to about {:.4} and the code carries almost no information",
        1.0 / (padded as f32).sqrt()
    );
}

#[test]
fn a_deliberately_wrong_estimator_breaches_the_bound() {
    // ⚠️ Without this, a bound so loose that anything satisfies it passes the test above,
    // and "RaBitQ has an error bound" becomes a sentence rather than a property. The wrong
    // estimator drops the normalisation by <x̄,x>, which is the one term that turns a
    // biased sign-vector inner product into an unbiased estimate.
    //
    // ⚠️ NEAR-NEIGHBOUR queries, unlike the test above, and the difference is not
    // incidental. The missing normalisation is a MULTIPLICATIVE bias of about 20%, so in
    // absolute terms it is invisible when <x,q> is near zero -- which is what two random
    // unit vectors in 512 dimensions give you. It only exceeds the bound when the query is
    // actually close to the vector, which is the only case an ANN index ever serves. Built
    // on random pairs this test passes against a knowingly broken estimator.
    let q = Quantizer::new(DIM);
    let mut rng = Rng(SEED);
    let mut breaches = 0usize;
    let n = 2_000;
    for _ in 0..n {
        let mut o = rng.vector(DIM);
        normalize(&mut o);
        let query = near(&o, &mut rng, 0.5);
        let code = q.encode(&o).unwrap();
        let wrong = q.estimate_unnormalized_for_test(&code, &q.prepare(&query).unwrap());
        if (wrong - dot(&o, &query)).abs() > q.error_bound(&code) {
            breaches += 1;
        }
    }
    assert!(
        breaches * 10 > n,
        "an estimator missing its normalisation breached the bound only {breaches} times \
         in {n}: the bound is too loose to mean anything"
    );
}

#[test]
fn the_alignment_is_measured_per_vector_not_assumed() {
    // ⚠️ For a random vector in high dimension the alignment concentrates hard around
    // sqrt(2/π) ≈ 0.798 -- so hardcoding that constant passes every other test in this
    // file, saves four bytes a vector, and is wrong. It is wrong because the bound is a
    // function of the alignment: a vector the sign pattern happens to represent poorly gets
    // a wider bound, and one it represents well gets a tighter one. Replace the measurement
    // with the average and both are told the same lie.
    //
    // Low dimension is where the concentration is loose enough to see, which is also the
    // regime D-10 says most indexes live in.
    let dim = 16;
    let q = Quantizer::new(dim);
    let mut rng = Rng(3);
    let mut bounds: Vec<f32> = Vec::new();
    for _ in 0..200 {
        let mut v = rng.vector(dim);
        normalize(&mut v);
        bounds.push(q.error_bound(&q.encode(&v).unwrap()));
    }
    let lo = bounds.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = bounds.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    assert!(
        hi - lo > 0.05,
        "every vector got essentially the same bound ({lo:.4}..{hi:.4}): the alignment is \
         being assumed rather than measured, so the bound no longer describes the code it \
         is attached to"
    );
}

#[test]
fn a_dimension_mismatch_is_an_error_not_a_wrong_answer() {
    let q = Quantizer::new(DIM);
    assert!(q.encode(&[0.0; 8]).is_err());
    let code = q.encode(&vec![0.1; DIM]).unwrap();
    assert!(q.estimate(&code, &[0.0; 8]).is_err());
}

#[test]
fn the_rotation_beats_no_rotation_on_structured_input() {
    // ⚠️ The OQ-39 finding, as a test rather than only a number in an example. BBQ's
    // construction is this one without the randomized transform — both subtract a centroid
    // and keep per-vector corrections — and the rotation is what separates them: +5.7 points
    // of recall@10 at rung 0, measured. Here the same difference shows as code quality on
    // input that has structure, which real embeddings do.
    //
    let dim = 256;
    let rotated = Quantizer::new(dim);
    let plain = Quantizer::without_rotation(dim);

    // ⚠️ Structured means **concentrated**, not alternating. A first attempt used a ±1
    // square wave and measured unrotated alignment of 1.0000 — its best case, because every
    // sign is exact when every coordinate has the same magnitude. What an unrotated scheme
    // cannot handle is energy in a few coordinates: the signs of the near-zero rest are
    // noise, and the code describes almost nothing. Real embeddings are concentrated far
    // more often than they are alternating.
    let mut worst_rotated = 1.0f32;
    let mut worst_plain = 1.0f32;
    for spike in [1usize, 2, 4] {
        let mut v: Vec<f32> = (0..dim)
            .map(|i| if i < spike { 1.0 } else { 0.001 })
            .collect();
        normalize(&mut v);
        worst_rotated = worst_rotated.min(rotated.encode(&v).unwrap().alignment());
        worst_plain = worst_plain.min(plain.encode(&v).unwrap().alignment());
    }
    assert!(
        worst_rotated > worst_plain * 2.0,
        "rotated alignment {worst_rotated:.4} against unrotated {worst_plain:.4}: the \
         transform is not earning its cost on concentrated input"
    );
    assert!(worst_rotated > 0.5, "even rotated, alignment collapsed");
}

#[test]
fn an_int4_query_costs_little_accuracy() {
    // ⚠️ BBQ's asymmetry: 1-bit documents scored against a low-precision query. Our query is
    // `f32`, which is *more* accurate, so int4 is a throughput option rather than an
    // accuracy one — worth taking when SIMD lands. Measured at −1.3 points of recall@10;
    // here the same claim is pinned as estimate error, which needs no corpus.
    let q = Quantizer::new(DIM);
    let mut rng = Rng(23);
    let (mut full, mut quad) = (0.0f64, 0.0f64);
    let n = 500;
    for _ in 0..n {
        let mut o = rng.vector(DIM);
        normalize(&mut o);
        let query = near(&o, &mut rng, 0.5);
        let code = q.encode(&o).unwrap();
        let prepared = q.prepare(&query).unwrap();
        let truth = dot(&o, &query);
        full += f64::from((q.estimate_prepared(&code, &prepared) - truth).abs());
        let int4 = q.quantize_query_int4(&prepared);
        quad += f64::from((q.estimate_prepared(&code, &int4) - truth).abs());
    }
    let (full, quad) = (full / f64::from(n), quad / f64::from(n));
    assert!(
        quad < full * 1.5,
        "int4 query error {quad:.5} against f32's {full:.5}: the asymmetry costs more than \
         a throughput trade should"
    );
}
