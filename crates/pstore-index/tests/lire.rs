//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Invariants of LIRE-style maintenance. The *verdict* on whether it preserves quality is
//! `examples/lire.rs`; these are the properties that must hold for that verdict to mean
//! anything.

use pstore_index::cluster::{Clustering, Params};
use pstore_index::lire::{self, Bounds, Scope};

const DIM: usize = 32;

struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    fn normal(&mut self) -> f32 {
        (0..6).map(|_| self.f()).sum()
    }
}

fn corpus(n: usize, groups: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..groups)
        .map(|_| (0..DIM).map(|_| rng.normal() * 1.5).collect())
        .collect();
    (0..n)
        .map(|i| {
            let mut v: Vec<f32> = centres[i % groups]
                .iter()
                .map(|x| x + rng.normal())
                .collect();
            let m: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            for x in v.iter_mut() {
                *x /= m;
            }
            v
        })
        .collect()
}

/// ⚠️ `replicas: 0`. Maintenance operates on the **assignment**, not on the augmented
/// lists: boundary replication is a build-time step applied when the segment is written, and
/// a replica has no home centroid to be reassigned from. Feeding augmented lists to
/// `maintain` also makes every centroid move on the first recentre — `Clustering::build`
/// computes centroids from the assignment and then adds replicas — so every list looks
/// disturbed and the scope restriction stops paying.
fn params() -> Params {
    Params {
        target_list_size: 50,
        replicas: 0,
        ..Params::default()
    }
}

fn all_rows(c: &Clustering) -> Vec<usize> {
    let mut v: Vec<usize> = c.lists().iter().flatten().copied().collect();
    v.sort_unstable();
    v
}

#[tokio::test]
async fn maintenance_loses_no_vector_and_duplicates_none() {
    // ⚠️ The failure that matters most and shows least. A row dropped during a split, a
    // merge or a reassignment is unreachable at any probe width, and surfaces only as a
    // recall ceiling nothing explains. A row duplicated can occupy two slots of a top-k.
    let corpus = corpus(1_200, 10, 1);
    let mut c = Clustering::build(&corpus[..800], params());
    let mut at = 800;
    for _ in 0..4 {
        let rows: Vec<usize> = (at..at + 100).collect();
        at += 100;
        lire::maintain(&mut c, &corpus[..at], &rows, params(), Scope::Touched);
        let seen = all_rows(&c);
        assert_eq!(seen.len(), at, "expected {at} rows, found {}", seen.len());
        assert_eq!(
            seen,
            (0..at).collect::<Vec<_>>(),
            "a row was lost or duplicated"
        );
    }
}

#[tokio::test]
async fn an_overgrown_list_is_split() {
    // Without splitting, inserts pile into whichever list is nearest and one probe ends up
    // reading a multiple of the byte budget.
    let corpus = corpus(2_000, 3, 2);
    let mut c = Clustering::build(&corpus[..300], params());
    let before = c.lists().len();
    let rows: Vec<usize> = (300..2_000).collect();
    let w = lire::maintain(&mut c, &corpus, &rows, params(), Scope::Touched);
    assert!(
        w.splits > 0,
        "1,700 inserts into {before} lists caused no split"
    );
    let bounds = Bounds::from_params(params());
    let largest = c.lists().iter().map(Vec::len).max().unwrap_or(0);
    assert!(
        largest <= bounds.max * 2,
        "largest list is {largest} against a split bound of {}",
        bounds.max
    );
}

#[tokio::test]
async fn touched_scope_examines_far_less_than_a_full_scan() {
    // ⚠️ The entire economic claim. If restricting reassignment to the disturbed lists does
    // not cost much less than scanning everything, LIRE is a rebuild in instalments.
    // ⚠️ A batch that is ~3% of the index, which is the regime the protocol is for
    // (`incremental-maintenance.md` quotes 1%/day). A batch that is half the index touches
    // nearly every list, "touched" collapses onto "all", and the test measures nothing —
    // measured at 50%, the saving was 15%.
    let corpus = corpus(3_000, 12, 3);
    let seed: Vec<Vec<f32>> = corpus[..2_900].to_vec();
    let rows: Vec<usize> = (2_900..3_000).collect();

    let mut touched = Clustering::build(&seed, params());
    let a = lire::maintain(&mut touched, &corpus, &rows, params(), Scope::Touched);
    let mut full = Clustering::build(&seed, params());
    let b = lire::maintain(&mut full, &corpus, &rows, params(), Scope::All);

    assert!(a.examined > 0, "the touched scope examined nothing at all");
    assert!(
        a.examined * 2 < b.examined,
        "touched examined {} against a full scan's {}: the scope restriction is not paying",
        a.examined,
        b.examined
    );
}

#[tokio::test]
async fn maintenance_is_deterministic() {
    // Drift is measured across runs; a maintenance pass that varied would make every
    // comparison noise rather than evidence.
    let corpus = corpus(1_500, 8, 4);
    let seed: Vec<Vec<f32>> = corpus[..1_000].to_vec();
    let rows: Vec<usize> = (1_000..1_500).collect();
    let run = || {
        let mut c = Clustering::build(&seed, params());
        let w = lire::maintain(&mut c, &corpus, &rows, params(), Scope::Touched);
        (c.lists().to_vec(), w)
    };
    assert_eq!(run(), run(), "two identical passes disagreed");
}

#[tokio::test]
async fn maintenance_converges_rather_than_churning() {
    // ⚠️ An earlier version of this asserted that an empty batch changes nothing. That is
    // false, and usefully so: `Clustering::build` leaves centroids that are the mean of the
    // assignment BEFORE the last one, so a fresh index carries a little drift and the first
    // maintenance pass legitimately repairs it.
    //
    // What must hold is the weaker, real property: repair CONVERGES. A pass that keeps
    // finding work on an unchanged corpus is a pass that oscillates, and it would rewrite a
    // segment every cycle for nothing.
    let corpus = corpus(600, 6, 5);
    let mut c = Clustering::build(&corpus, params());
    let first = lire::maintain(&mut c, &corpus, &[], params(), Scope::Touched);
    let settled = c.lists().to_vec();
    let second = lire::maintain(&mut c, &corpus, &[], params(), Scope::Touched);

    assert_eq!(first.inserted, 0);
    assert_eq!(second.inserted, 0);
    assert!(
        second.reassigned * 200 <= corpus.len(),
        "the second pass over an unchanged corpus moved {} of {} rows: maintenance \
         oscillates, so it would rewrite a segment every cycle for nothing",
        second.reassigned,
        corpus.len()
    );
    assert!(
        second.reassigned < first.reassigned.max(1),
        "the second pass did no less work than the first"
    );
    let _ = settled;
}

#[tokio::test]
async fn the_split_bound_is_not_the_balance_bound() {
    // They answer different questions, and tying them together let lists grow to 4x the
    // target -- same recall, bigger reads on every probe. Measured in `examples/lire.rs`.
    let p = params();
    assert_eq!(Bounds::from_params(p).max, p.target_list_size * 2);
    assert_ne!(
        Bounds::from_params(p).max,
        (p.target_list_size as f32 * p.balance) as usize
    );
}
