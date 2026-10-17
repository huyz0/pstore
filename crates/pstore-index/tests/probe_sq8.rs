//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! An int8 centroid table (M97, OQ-171): does it probe as the `f32` table does?

use pstore_index::cluster::Clustering;
use pstore_index::search;
use pstore_index::sq8;

const DIM: usize = 64;

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
    fn direction(&mut self) -> Vec<f32> {
        let v: Vec<f32> = (0..DIM)
            .map(|_| (0..6).map(|_| self.unit()).sum())
            .collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / n).collect()
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn sq8_probe_orders_and_scores_as_f32() {
    // 56 centroids at distances 0.2, 0.3, ... from the query, in a shuffled index order so
    // that ranking by index cannot pass. A centroid is `q + d·u` for a random unit `u` not
    // orthogonal to `q`, so `<c, q>` alone orders them differently from L2: ranking with
    // `||c||²` dropped fails. The last eight get a twin 1e-5 away, which sq8 may order
    // either way round: those eight pairs are the ties.
    let mut rng = Rng(3);
    let q = rng.direction();
    let mut centroids = vec![Vec::new(); 64];
    let mut class = vec![0usize; 64];
    for i in 0..56 {
        // A fixed permutation of 0..56 (37 is coprime to 56).
        let slot = (i * 37 + 11) % 56;
        let d = 0.2 + 0.1 * i as f32;
        let u = rng.direction();
        let c: Vec<f32> = q.iter().zip(&u).map(|(a, b)| a + d * b).collect();
        centroids[slot] = c;
        class[slot] = i;
    }
    // Slots 0..56 are filled; 56..64 are the twins of the last eight, 1e-5 away.
    for t in 0..8 {
        let twin_of = (0..56).find(|s| class[*s] == 48 + t).unwrap();
        let mut c = centroids[twin_of].clone();
        c[t] += 1e-5;
        centroids[56 + t] = c;
        class[56 + t] = 48 + t;
    }
    let table: Vec<sq8::Code> = centroids.iter().map(|c| sq8::encode(c)).collect();
    let clustering = Clustering::from_parts(centroids.clone(), vec![Vec::new(); 64]);

    for p in [64usize, 10] {
        let f32_order = search::probe(&clustering, &q, p);
        let sq8_probe = search::probe_sq8(&table, &q, p);
        assert_eq!(sq8_probe.len(), p);
        // Equal up to a tie: the class sequences match.
        let a: Vec<usize> = f32_order.iter().map(|i| class[*i]).collect();
        let b: Vec<usize> = sq8_probe.iter().map(|(i, _)| class[*i]).collect();
        assert_eq!(a, b, "p={p}: the int8 table probes in a different order");
    }
    // `<c, q>` within the int8 reconstruction bound: half a step on every coordinate.
    let q1: f32 = q.iter().map(|x| x.abs()).sum();
    for (i, cq) in search::probe_sq8(&table, &q, 64) {
        let bound = 0.5 * table[i].step() * q1 + 1e-5;
        let truth = dot(&centroids[i], &q);
        assert!(
            (cq - truth).abs() <= bound,
            "list {i}: <c, q> {cq} from the int8 table, {truth} in f32, bound {bound}"
        );
    }
}
