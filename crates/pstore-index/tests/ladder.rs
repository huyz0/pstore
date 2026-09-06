//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The rerank ladder (D-11, D-12).
//!
//! Recall is recovered by oversampling cheaply and then re-scoring the survivors at
//! increasing precision. The property that makes the ladder worth having is not that any
//! rung is accurate, but that **each rung is more accurate than the one below it** — if
//! int8 is not better than 1-bit, rung 1 is bytes and a round trip spent on nothing.

use pstore_index::ladder::{Ladder, Rung};
use pstore_index::rabitq::Quantizer;
use pstore_index::sq8;

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
    fn vector(&mut self, d: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..d).map(|_| self.normal()).collect();
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in v.iter_mut() {
            *x /= n;
        }
        v
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn int8_reconstructs_within_one_quantisation_step() {
    // The floor on what rung 1 can possibly know. Asserted directly so a later change to
    // the scale or offset shows up here rather than as a recall number nobody can explain.
    let mut rng = Rng(1);
    for _ in 0..200 {
        let v = rng.vector(DIM);
        let c = sq8::encode(&v);
        let back = sq8::decode(&c);
        let step = c.step();
        for (a, b) in v.iter().zip(&back) {
            // ⚠️ HALF a step, which is the bound for round-to-nearest. Asserting a whole
            // step is the loose bound that TRUNCATION also satisfies -- and truncation
            // shifts every coordinate down by about half a step, biasing the reconstructed
            // vector by a constant offset in every dimension at once. That mostly cancels
            // in an inner product against a zero-mean query, so no accuracy test catches
            // it; the tight bound here is what does.
            assert!(
                (a - b).abs() <= step * 0.51,
                "reconstructed {b} from {a}, step {step}: further than half a step, so the \
                 codes are truncated rather than rounded"
            );
        }
    }
}

#[test]
fn int8_rerank_beats_one_bit_on_the_same_pairs() {
    // ⚠️ The reason rung 1 exists. If this does not hold, the ladder is a round trip and a
    // section of every segment spent to learn nothing, and D-11's ladder should collapse to
    // rung 0 plus exact.
    let q = Quantizer::new(DIM);
    let mut rng = Rng(7);
    let (mut e0, mut e1) = (0.0f64, 0.0f64);
    let n = 2_000;
    for _ in 0..n {
        let (o, query) = (rng.vector(DIM), rng.vector(DIM));
        let truth = dot(&o, &query);
        e0 += f64::from((q.estimate(&q.encode(&o).unwrap(), &query).unwrap() - truth).abs());
        e1 += f64::from((sq8::estimate(&sq8::encode(&o), &query) - truth).abs());
    }
    let (m0, m1) = (e0 / f64::from(n), e1 / f64::from(n));
    assert!(
        m1 < m0 / 4.0,
        "int8 mean error {m1:.6} is not decisively better than 1-bit {m0:.6}; rung 1 is not \
         paying for its bytes"
    );
}

#[test]
fn the_ladder_narrows_and_never_widens() {
    // Each rung hands the next a strictly smaller candidate set, or the "ladder" is a
    // rescan at higher cost.
    let l = Ladder::new(10, 8);
    assert_eq!(l.keep(Rung::OneBit), 80);
    assert_eq!(l.keep(Rung::Int8), 20);
    assert_eq!(l.keep(Rung::Exact), 10);
    assert!(l.keep(Rung::OneBit) > l.keep(Rung::Int8));
    assert!(l.keep(Rung::Int8) > l.keep(Rung::Exact));
}

#[test]
fn a_higher_rung_never_returns_a_worse_top_k() {
    // ⚠️ The property that makes the knob honest (D-12): a client that pays for `fast` or
    // `exact` must not get a worse answer than one that paid for nothing. Measured as
    // agreement with brute force over many queries, not on a single lucky one.
    let q = Quantizer::new(DIM);
    let mut rng = Rng(11);
    let corpus: Vec<Vec<f32>> = (0..400).map(|_| rng.vector(DIM)).collect();
    let bits: Vec<_> = corpus.iter().map(|v| q.encode(v).unwrap()).collect();
    let eights: Vec<_> = corpus.iter().map(|v| sq8::encode(v)).collect();
    let l = Ladder::new(10, 8);

    let (mut hit0, mut hit1, mut hit2) = (0usize, 0usize, 0usize);
    for _ in 0..40 {
        let query = rng.vector(DIM);
        let mut exact: Vec<(usize, f32)> = corpus
            .iter()
            .enumerate()
            .map(|(i, v)| (i, dot(v, &query)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1));
        let want: Vec<usize> = exact.iter().take(10).map(|(i, _)| *i).collect();

        let prepared = q.prepare(&query).unwrap();
        let r0 = l.rung0(
            bits.iter()
                .enumerate()
                .map(|(i, c)| (i, q.estimate_prepared(c, &prepared))),
        );
        let r1 = l.rung1(&r0, |i| sq8::estimate(&eights[i], &query));
        let r2 = l.rung2(&r1, |i| dot(&corpus[i], &query));

        hit0 += want
            .iter()
            .filter(|i| r0.iter().take(10).any(|(j, _)| j == *i))
            .count();
        hit1 += want
            .iter()
            .filter(|i| r1.iter().take(10).any(|(j, _)| j == *i))
            .count();
        hit2 += want
            .iter()
            .filter(|i| r2.iter().take(10).any(|(j, _)| j == *i))
            .count();
    }
    assert!(hit1 >= hit0, "int8 rerank lost ground: {hit1} vs {hit0}");
    assert!(hit2 >= hit1, "exact rerank lost ground: {hit2} vs {hit1}");
    // ⚠️ NOT 400. The ladder is lossy by construction: rung 2 can only re-score what rung 0
    // passed up, so a true neighbour the 1-bit rung dropped is gone for good however exact
    // the rerank is. That is the trade oversample buys, and asserting losslessness here
    // would be asserting the ladder does not do its job. What it must be is *good*: 400
    // is the total across 40 queries of 10, so this is recall@10 at oversample 8.
    assert!(
        hit2 * 100 >= 400 * 90,
        "recall@10 through the full ladder was {}%, under the 90% floor",
        hit2 * 100 / 400
    );
}

#[test]
fn the_exact_rung_reproduces_brute_force_on_what_it_is_given() {
    // Rung 2 in isolation, handed the whole corpus. If "exact" is not exact on its own
    // input then D-12's knob promises something it cannot deliver, and no amount of
    // oversampling below it helps.
    let mut rng = Rng(17);
    let corpus: Vec<Vec<f32>> = (0..200).map(|_| rng.vector(DIM)).collect();
    let l = Ladder::new(10, 20);
    for _ in 0..20 {
        let query = rng.vector(DIM);
        let mut exact: Vec<(usize, f32)> = corpus
            .iter()
            .enumerate()
            .map(|(i, v)| (i, dot(v, &query)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let want: Vec<usize> = exact.iter().take(10).map(|(i, _)| *i).collect();

        let all: Vec<(usize, f32)> = (0..corpus.len()).map(|i| (i, 0.0)).collect();
        let got: Vec<usize> = l
            .rung2(&all, |i| dot(&corpus[i], &query))
            .iter()
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(got, want, "the exact rung disagreed with brute force");
    }
}

#[test]
fn int8_never_drops_a_true_neighbour_out_of_reach_of_the_exact_rung() {
    // ⚠️ Rung 1 is the ladder's real risk. It narrows 80 candidates to 20, and anything it
    // misjudges is gone before the exact rung ever sees it -- so its accuracy is not a
    // nice-to-have, it is a hard filter on what the exact rung can possibly return. Tested
    // with rung 0 keeping the whole corpus, so any loss here is int8's alone.
    let q = Quantizer::new(DIM);
    let mut rng = Rng(13);
    let corpus: Vec<Vec<f32>> = (0..200).map(|_| rng.vector(DIM)).collect();
    let bits: Vec<_> = corpus.iter().map(|v| q.encode(v).unwrap()).collect();
    let eights: Vec<_> = corpus.iter().map(|v| sq8::encode(v)).collect();
    // k=10, oversample 20 => rung 0 keeps 200, the whole corpus.
    let l = Ladder::new(10, 20);

    for _ in 0..20 {
        let query = rng.vector(DIM);
        let mut exact: Vec<(usize, f32)> = corpus
            .iter()
            .enumerate()
            .map(|(i, v)| (i, dot(v, &query)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let want: Vec<usize> = exact.iter().take(10).map(|(i, _)| *i).collect();

        let prepared = q.prepare(&query).unwrap();
        let r0 = l.rung0(
            bits.iter()
                .enumerate()
                .map(|(i, c)| (i, q.estimate_prepared(c, &prepared))),
        );
        assert_eq!(
            r0.len(),
            corpus.len(),
            "the oversample did not keep everything"
        );
        let r1 = l.rung1(&r0, |i| sq8::estimate(&eights[i], &query));
        let r2 = l.rung2(&r1, |i| dot(&corpus[i], &query));
        let got: Vec<usize> = r2.iter().map(|(i, _)| *i).collect();
        assert_eq!(got, want, "exact rerank disagreed with brute force");
    }
}

#[test]
fn a_constant_vector_quantises_without_dividing_by_zero() {
    // A vector whose coordinates are all equal has no range, so the step is zero and every
    // code is the same. The reconstruction is exact and the estimate is well-defined; the
    // only thing that could go wrong is dividing by the range, which is why the guard exists
    // and why this exercises it.
    let v = vec![0.25f32; 32];
    let c = sq8::encode(&v);
    assert_eq!(c.step(), 0.0);
    assert_eq!(sq8::decode(&c), v, "a constant vector did not round-trip");
    let q = vec![1.0f32; 32];
    assert!((sq8::estimate(&c, &q) - 8.0).abs() < 1e-4);

    // And through the wire form, which is what a segment stores.
    let mut raw = Vec::new();
    sq8::write_to(&c, &mut raw);
    assert_eq!(raw.len(), sq8::record_len(32));
    assert!((sq8::estimate_raw(&raw, &q) - 8.0).abs() < 1e-4);
}
