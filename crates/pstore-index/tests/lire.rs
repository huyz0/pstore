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

#[tokio::test]
async fn a_starved_list_is_merged_into_its_neighbour() {
    // The other half of the split rule. Without merging, deletes and drift leave a tail of
    // near-empty lists, each costing a centroid, a directory entry and a probe that can
    // barely return anything -- so a query's byte budget buys less and less over time.
    let corpus = corpus(600, 4, 11);
    // Many tiny lists to start with, so the merge rule has something to do.
    let tiny = Params {
        target_list_size: 8,
        replicas: 0,
        ..Params::default()
    };
    let mut c = Clustering::build(&corpus, tiny);
    let before = c.lists().len();
    assert!(before > 20, "only {before} lists to merge from");

    // Maintaining under the normal target makes most of those lists starved.
    let w = lire::maintain(&mut c, &corpus, &[], params(), Scope::All);
    assert!(
        w.merges > 0,
        "{before} lists far under the target produced no merges"
    );
    assert!(
        c.lists().len() < before,
        "merging did not reduce the list count"
    );
    // And nothing was lost on the way.
    let seen: usize = c.lists().iter().map(Vec::len).sum();
    assert_eq!(seen, corpus.len(), "a merge lost or duplicated rows");
}

#[tokio::test]
async fn maintenance_survives_the_degenerate_cases() {
    // The branches a real corpus never reaches and a bad one does. None of these should
    // panic, lose rows, or produce a clustering that later code cannot read.
    let p = params();

    // No centroids at all: nothing to maintain, and nothing to crash on.
    let mut empty = Clustering::from_parts(Vec::new(), Vec::new());
    let w = lire::maintain(&mut empty, &[], &[], p, Scope::Touched);
    assert_eq!(w, Default::default());

    // A list of IDENTICAL vectors cannot be bisected — every point is the farthest point
    // from every other, so the split rule has nothing to divide on. It must leave the list
    // alone rather than emit an empty half.
    let same: Vec<Vec<f32>> = (0..400).map(|_| vec![1.0f32; DIM]).collect();
    let mut c = Clustering::build(&same, p);
    let before: usize = c.lists().iter().map(Vec::len).sum();
    lire::maintain(&mut c, &same, &[], p, Scope::All);
    let after: usize = c.lists().iter().map(Vec::len).sum();
    assert_eq!(after, before, "an unsplittable list lost or gained rows");
    assert!(
        c.lists().iter().all(|l| !l.is_empty()),
        "an empty list was created"
    );

    // An insert naming a row that does not exist is ignored rather than panicking: the
    // caller's row list and corpus can disagree, and a panic in maintenance takes down a
    // compaction rather than skipping a document.
    let corpus = corpus(200, 4, 31);
    let mut c = Clustering::build(&corpus, p);
    let w = lire::maintain(&mut c, &corpus, &[9_999], p, Scope::Touched);
    assert_eq!(w.inserted, 0, "a row outside the corpus was inserted");
}

#[test]
fn the_split_factor_can_be_set_explicitly() {
    // The sweep in `examples/lire.rs` chose 2x by measuring 4x, 2x and 1.5x; the knob it
    // swept must keep working, or that measurement cannot be repeated.
    let p = params();
    assert_eq!(
        Bounds::with_split_factor(p, 3.0).max,
        p.target_list_size * 3
    );
    assert_eq!(
        Bounds::with_split_factor(p, 1.5).max,
        (p.target_list_size as f32 * 1.5) as usize
    );
}

#[test]
fn the_merge_bound_is_a_quarter_of_the_target() {
    // ⚠️ **`min` was asserted nowhere.** `the_split_factor_can_be_set_explicitly` and
    // `the_split_bound_is_not_the_balance_bound` both pin `max` and neither looks at `min`, so
    // a mutation sweep found `target_list_size / 4` indistinguishable from `% 4` and from
    // `* 4`. A merge bound of `target * 4` merges every list into its neighbour on the first
    // pass; `% 4` gives 0 for most targets, clamped to 1, so nothing ever merges. Both are
    // silent: the index still answers, with a partition nobody chose.
    for target in [4usize, 40, 100, 4_000] {
        let p = Params {
            target_list_size: target,
            ..Params::default()
        };
        let b = Bounds::from_params(p);
        assert_eq!(b.min, (target / 4).max(1), "target={target}");
        assert!(b.min < b.max, "target={target}: the bounds crossed");
        assert_eq!(Bounds::with_split_factor(p, 2.0).min, b.min);
    }
    // ⚠️ The clamp, which is the case `% 4` and `/ 4` agree on and `* 4` does not.
    let tiny = Params {
        target_list_size: 1,
        ..Params::default()
    };
    assert_eq!(
        Bounds::from_params(tiny).min,
        1,
        "a bound of zero merges forever"
    );
}

#[tokio::test]
async fn a_split_names_only_lists_that_exist() {
    // A split rewrites list `i` in place and pushes the other half. Off by one and a half is
    // lost, duplicated or left empty -- recall drifts and nothing errors.
    //
    // ⚠️ This comment once said `split_pass` marked both halves dirty and that set was the
    // reassignment scope. It was not: the scope is recomputed from centroid movement after the
    // merges, and the set was written and never read. M8j deleted it.
    let corpus = corpus(3_000, 12, 5);
    let seed: Vec<Vec<f32>> = corpus[..2_600].to_vec();
    let rows: Vec<usize> = (2_600..3_000).collect();
    let mut c = Clustering::build(&seed, params());
    let before = c.lists().len();
    let work = lire::maintain(&mut c, &corpus, &rows, params(), Scope::Touched);
    assert!(work.splits > 0, "the fixture split nothing");
    assert!(c.lists().len() > before);

    // Every vector is in exactly one list, and every list is non-empty — which is what a
    // dirty index naming a phantom list, or a split producing an empty half, breaks.
    let mut seen = vec![0usize; corpus.len()];
    for (i, list) in c.lists().iter().enumerate() {
        assert!(!list.is_empty(), "list {i} is empty after a split");
        for r in list {
            seen[*r] += 1;
        }
    }
    assert!(
        seen.iter().all(|n| *n == 1),
        "a vector is in {} lists after a split",
        seen.iter().copied().max().unwrap_or(0)
    );
}

#[tokio::test]
async fn an_unsplittable_list_over_the_bound_is_left_whole() {
    // ⚠️ `maintenance_survives_the_degenerate_cases` builds 400 identical vectors and asserts
    // no empty list — but with `target_list_size: 50` the clustering spreads them across lists
    // of 50, none over the split bound of 100, so `split_pass` never runs and the assertion is
    // vacuous. A mutation sweep proved it: `a.is_empty() || b.is_empty()` mutated to `&&`
    // survived, and that mutation pushes an EMPTY list whenever a bisect degenerates.
    //
    // Constructed directly, so the list is over the bound and unsplittable by construction.
    let p = params();
    let rows: Vec<usize> = (0..200).collect();
    let same: Vec<Vec<f32>> = (0..200).map(|_| vec![0.5f32; DIM]).collect();
    let mut c = Clustering::from_parts(vec![vec![0.5f32; DIM]], vec![rows]);
    assert!(
        c.lists()[0].len() > Bounds::from_params(p).max,
        "the fixture is under the split bound, so nothing is attempted"
    );

    let work = lire::maintain(&mut c, &same, &[], p, Scope::All);
    assert_eq!(work.splits, 0, "an unsplittable list was split anyway");
    assert!(
        c.lists().iter().all(|l| !l.is_empty()),
        "a degenerate bisect pushed an empty list: {:?}",
        c.lists().iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert_eq!(
        c.lists().iter().map(Vec::len).sum::<usize>(),
        200,
        "the unsplittable list lost or gained rows"
    );
}

#[tokio::test]
async fn a_splits_halves_are_reassigned_to_their_own_centroids() {
    // After a split, every vector should sit in the list whose centroid is nearest it -- or
    // the partition is quietly worse than the one the protocol claims, with no error and no
    // lost row.
    let corpus = corpus(3_000, 12, 11);
    let seed: Vec<Vec<f32>> = corpus[..2_600].to_vec();
    let rows: Vec<usize> = (2_600..3_000).collect();
    let mut c = Clustering::build(&seed, params());
    let work = lire::maintain(&mut c, &corpus, &rows, params(), Scope::Touched);
    assert!(work.splits > 0, "the fixture split nothing");

    let cents = c.centroids().to_vec();
    let mut misplaced = 0usize;
    let mut total = 0usize;
    for (li, list) in c.lists().iter().enumerate() {
        for r in list {
            total += 1;
            let v = &corpus[*r];
            let d = |c: &Vec<f32>| -> f32 { v.iter().zip(c).map(|(a, b)| (a - b) * (a - b)).sum() };
            let own = d(&cents[li]);
            if cents.iter().any(|c| d(c) < own - 1e-6) {
                misplaced += 1;
            }
        }
    }
    // ⚠️ Not zero: `Scope::Touched` deliberately reconsiders only the disturbed lists, so
    // vectors in untouched lists may drift. Measured at **82 of 3,000**.
    //
    // ⚠️ `dirty.insert(lists.len() - 1)` mutated to `+ 1` measured 82 of 3,000 either way,
    // and was recorded here as inert and not chased. It was inert because the set was never
    // READ -- the scope is `dirty_now`, from centroid movement -- and M8j deleted it.
    assert!(
        misplaced * 20 < total,
        "{misplaced} of {total} vectors are nearer another centroid — the split's halves were \
         not reassigned"
    );
}

#[tokio::test]
async fn the_work_report_counts_what_actually_happened() {
    // ⚠️ **`Work` is the spike's OUTPUT.** `examples/lire.rs` reads it to answer OQ-51 — the
    // whole point of the module is to report cost, and the fields' own doc says so: "the
    // number the whole claim rests on". A counter that never increments answers the open
    // question with a zero, and nothing here asserted any of them was non-zero: a sweep found
    // `inserted += 1` and `reassigned += moves.len()` both indistinguishable from `*=`, which
    // pins them at 0 forever because they start there.
    let corpus = corpus(3_000, 12, 17);
    let seed: Vec<Vec<f32>> = corpus[..2_500].to_vec();
    let rows: Vec<usize> = (2_500..3_000).collect();
    let mut c = Clustering::build(&seed, params());
    let w = lire::maintain(&mut c, &corpus, &rows, params(), Scope::Touched);

    assert_eq!(
        w.inserted,
        rows.len(),
        "every row handed in was appended, or the count is not counting"
    );
    assert!(w.splits > 0, "a 20% batch split nothing");
    assert!(
        w.examined > 0,
        "nothing was examined, so nothing was reassigned either"
    );
    assert!(
        w.reassigned > 0,
        "no vector changed list after {} splits — the churn counter is stuck at zero",
        w.splits
    );
    // ⚠️ Reassigned counts vectors that MOVED; examined counts vectors looked at to find
    // them. One cannot exceed the other, and a counter incrementing the wrong variable
    // shows up here rather than as a plausible pair of numbers.
    assert!(
        w.reassigned <= w.examined,
        "reassigned {} of {} examined",
        w.reassigned,
        w.examined
    );
    // ⚠️ Inverting `bisect`'s side test, or the reassignment's own `best != li`, cannot be
    // seen from THIS fixture: on the split path `bisect` places vectors well and the two
    // inversions leave outcomes this test does not distinguish. M8j catches both elsewhere,
    // with fixtures built for them: `bisect` pinned against a model of the documented
    // algorithm, and a clustering with one row deliberately misplaced.
    //
    // And the totals are consistent with the clustering that came out.
    assert_eq!(
        c.lists().iter().map(Vec::len).sum::<usize>(),
        corpus.len(),
        "the report does not describe the clustering it produced"
    );
}

#[tokio::test]
async fn a_list_at_the_split_bound_is_left_whole_and_one_past_it_is_split() {
    // The bound is "split ABOVE `max`" (`Bounds::max`'s own doc), and nothing reached it
    // exactly: every fixture overshoots it by hundreds of rows.
    let bounds = Bounds { max: 3, min: 1 };
    let line = |n: usize| -> Vec<Vec<f32>> { (0..n).map(|i| vec![i as f32, 0.0]).collect() };
    for (n, splits) in [(3usize, 0usize), (4, 1)] {
        let corpus = line(n);
        let mut c = Clustering::from_parts(vec![vec![1.0, 0.0]], vec![(0..n).collect()]);
        let w = lire::maintain_with(&mut c, &corpus, &[], params(), Scope::All, bounds);
        assert_eq!(w.splits, splits, "a list of {n} rows against a bound of 3");
    }
}

/// `Scope::Touched` over two single-row lists whose centroids start at the origin and far
/// away, the first row at `(2^-10, y)`. Returns what the pass examined.
fn examined_after_moving_to(y: f32) -> usize {
    let corpus = vec![vec![0.000_976_562_5, y], vec![10.0, 10.0]];
    let mut c = Clustering::from_parts(
        vec![vec![0.0, 0.0], vec![10.0, 10.0]],
        vec![vec![0], vec![1]],
    );
    let w = lire::maintain_with(
        &mut c,
        &corpus,
        &[],
        params(),
        Scope::Touched,
        Bounds { max: 10, min: 1 },
    );
    w.examined
}

#[tokio::test]
async fn a_centroid_that_moves_exactly_the_threshold_is_not_disturbed() {
    // `(2^-10, 0.031607694923877716)` is EXACTLY `SPLIT_DISTURBANCE` (1e-3) from the origin
    // in f32, summed in `dist2`'s order -- found by searching an f32 model; no 1-D value
    // squares to it. Recentring the single-row list moves its centroid there.
    // Exact bits, not a decimal: the test is about one specific float.
    let at = f32::from_bits(0x3D01_7712);
    assert_eq!(
        examined_after_moving_to(at),
        0,
        "a move of exactly the threshold disturbed the list"
    );
    // One float further, the list is disturbed -- and so is its nearest neighbour, which a
    // moved centroid can pull vectors from. Both single-row lists are examined.
    assert_eq!(
        examined_after_moving_to(at.next_up()),
        2,
        "a moved list, or its neighbour, was not re-examined"
    );
}

#[tokio::test]
async fn reassignment_moves_a_row_to_the_list_it_is_nearest() {
    // Recentred, the first list sits at 4.5 and the second at 10: row 1 (at 9) is nearer
    // the second, and a full-scope pass must move it there -- once.
    let corpus = vec![vec![0.0], vec![9.0], vec![10.0]];
    let mut c = Clustering::from_parts(vec![vec![0.0], vec![10.0]], vec![vec![0, 1], vec![2]]);
    let w = lire::maintain_with(
        &mut c,
        &corpus,
        &[],
        params(),
        Scope::All,
        Bounds { max: 10, min: 1 },
    );
    assert_eq!(c.lists(), &[vec![0], vec![1, 2]]);
    assert_eq!(w.reassigned, 1);
}
