//! LIRE-style incremental maintenance — **a spike for OQ-51**, not a shipped path.
//!
//! ## The question
//!
//! `06-indexing/incremental-maintenance.md` proposes maintaining a clustered index with
//! SPFresh's LIRE protocol: append on insert, split an overgrown posting list, merge a
//! starved one, and afterwards **reassign only the boundary vectors** the change disturbed.
//! Published at billion scale it beats global rebuild on latency and accuracy using 1% of
//! the DRAM and under 10% of the cores.
//!
//! ⚠️ That result is for a **mutable** disk index. Ours is immutable objects, so the
//! operations have to be batched into a segment rewrite, and the open question — the
//! design's biggest, per the corpus — is whether batching preserves partition quality or
//! whether the drift compounds until a rebuild is needed anyway.
//!
//! ## What this module is
//!
//! The operations, with their invariants tested, plus enough structure for
//! `examples/lire.rs` to measure drift against a rebuild. It is deliberately **not** wired
//! into `vec_index`: the spike answers a question before anything hardens around it, and
//! wiring it in first is what the roadmap put this milestone out of order to avoid.

use crate::cluster::{Clustering, Params};

/// How many split rounds before giving up. Each halves the largest list, so this covers a
/// list `2^12` times its bound.
const SPLIT_ROUNDS: usize = 12;
/// How many reassignment rounds before accepting the index as settled.
const REASSIGN_ROUNDS: usize = 8;
/// How far a centroid must move to count as disturbed.
///
/// ⚠️ Not `> 0`. Appending a row shifts a centroid by roughly `1/n` of a vector, which is
/// real movement and changes nobody's nearest; counting it dirties every list that received
/// an insert, which for any realistic batch is all of them.
const SPLIT_DISTURBANCE: f32 = 1e-3;

/// One split pass over every list that exceeds `bounds.max`.
fn split_pass(
    corpus: &[Vec<f32>],
    lists: &mut Vec<Vec<usize>>,
    centroids: &mut Vec<Vec<f32>>,
    dirty: &mut std::collections::BTreeSet<usize>,
    work: &mut Work,
    bounds: Bounds,
) {
    let mut i = 0;
    while i < lists.len() {
        if lists.get(i).is_some_and(|l| l.len() > bounds.max) {
            let Some(rows) = lists.get(i).cloned() else {
                i += 1;
                continue;
            };
            let (a, b, ca, cb) = bisect(corpus, &rows);
            if a.is_empty() || b.is_empty() {
                i += 1;
                continue;
            }
            if let (Some(list), Some(cen)) = (lists.get_mut(i), centroids.get_mut(i)) {
                *list = a;
                *cen = ca;
            }
            lists.push(b);
            centroids.push(cb);
            dirty.insert(i);
            dirty.insert(lists.len() - 1);
            work.splits += 1;
        }
        i += 1;
    }
}

/// When a posting list is too big or too small to leave alone.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// Split above this. Derived from the target list size, not set independently, so the
    /// two cannot drift apart.
    pub max: usize,
    /// Merge below this.
    pub min: usize,
}

impl Bounds {
    /// The bounds implied by clustering parameters, with an explicit split factor.
    ///
    /// ⚠️ `split_at` is **not** `balance`. Reusing the balance bound means a list may grow
    /// to 4x the target before splitting, so an incrementally-maintained index settles on
    /// far fewer, far larger lists than a rebuild would — same recall, but every probe
    /// reads a bigger list. The two numbers answer different questions and tying them
    /// together hides a cost in bytes behind a recall number that looks fine.
    #[must_use]
    pub fn with_split_factor(p: Params, split_at: f32) -> Self {
        Self {
            max: (p.target_list_size as f32 * split_at) as usize,
            min: (p.target_list_size / 4).max(1),
        }
    }

    /// The bounds implied by clustering parameters.
    #[must_use]
    pub fn from_params(p: Params) -> Self {
        Self {
            // ⚠️ 2x the target, **not** the balance bound. Measured: reusing balance (4x)
            // lets lists grow to 2.0x the mean before splitting, settling the index on 40
            // large lists where a rebuild makes 67 — same recall, but every probe reads a
            // bigger list, which is a cost the recall number hides. At 2x the index settles
            // at 46 lists with max/mean 1.32, the best balance of any factor tried, and
            // recall within 0.3 points.
            max: p.target_list_size * 2,
            // A quarter of target: low enough that a list has to be genuinely starved,
            // high enough that merging is not perpetually one insert away from splitting.
            min: (p.target_list_size / 4).max(1),
        }
    }
}

/// What a maintenance pass did, so the spike can report cost rather than assert it was low.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Work {
    /// Rows appended.
    pub inserted: usize,
    /// Lists divided.
    pub splits: usize,
    /// Lists absorbed into a neighbour.
    pub merges: usize,
    /// Vectors that actually changed list.
    pub reassigned: usize,
    /// ⚠️ **The number the whole claim rests on.** Vectors *examined* to find them — the
    /// work done, as distinct from the churn produced. LIRE is only cheaper than a rebuild
    /// because it examines the lists a split or merge disturbed rather than the corpus; if
    /// this approaches the corpus size the protocol has become a rebuild with extra steps.
    pub examined: usize,
}

/// How much of the index a maintenance pass is allowed to re-examine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Only the lists a split, merge or insert touched. This is LIRE.
    Touched,
    /// Every list. The reference — correct by construction, and the cost LIRE claims to
    /// avoid. Kept so the spike can measure what restricting the scope costs in quality.
    All,
}

/// Appends rows to their nearest list, then repairs whatever that broke.
///
/// Returns the work done. The caller supplies the **whole** corpus, including the new rows,
/// because a reassignment needs the vectors it is moving.
pub fn maintain(
    clustering: &mut Clustering,
    corpus: &[Vec<f32>],
    new_rows: &[usize],
    params: Params,
    scope: Scope,
) -> Work {
    maintain_with(
        clustering,
        corpus,
        new_rows,
        params,
        scope,
        Bounds::from_params(params),
    )
}

/// [`maintain`], with explicit bounds. For the spike, which sweeps them.
pub fn maintain_with(
    clustering: &mut Clustering,
    corpus: &[Vec<f32>],
    new_rows: &[usize],
    params: Params,
    scope: Scope,
    bounds: Bounds,
) -> Work {
    let mut work = Work::default();
    let _ = params;

    let mut centroids = clustering.centroids().to_vec();
    let mut lists = clustering.lists().to_vec();
    // Lists an edit disturbed. A vector's nearest centroid can only have changed if a
    // centroid near it moved, so this is the set LIRE re-examines -- and the reason it is
    // cheaper than a rebuild rather than a rebuild in instalments.
    let mut dirty: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    if centroids.is_empty() {
        return work;
    }

    // Insert: append to the nearest centroid. No capacity check -- LIRE's whole shape is
    // "let it grow, then split", because refusing the natural list is what a bounded
    // assignment does and it is exactly the quality loss a rebuild avoids.
    for row in new_rows {
        let Some(v) = corpus.get(*row) else { continue };
        let target = nearest(&centroids, v);
        if let Some(l) = lists.get_mut(target) {
            l.push(*row);
            work.inserted += 1;
            // ⚠️ An insert does NOT dirty the list. `incremental-maintenance.md` triggers
            // reassignment on split and merge — "after a split/merge, re-check only
            // boundary vectors" — because appending a row moves its centroid by 1/n and
            // changes nobody else's nearest. Dirtying on insert makes every list dirty for
            // any batch that touches most lists, which at realistic batch sizes is every
            // batch, and the scope restriction stops paying: measured 1.36x against a full
            // scan instead of 4x.
        }
    }

    // ⚠️ Split until nothing exceeds the bound, not once. A single pass was wrong on its
    // own comment's reasoning: bisecting a 1,700-row list gives two lists of ~850, both
    // still over a bound of 100, and the pass has already moved past them. Measured: the
    // largest list came out at 335 against a bound of 100.
    //
    // Bounded rather than `loop`: each round at least halves the largest list, so the bound
    // is log2 of the corpus, and an unbounded rewrite loop is a liveness bug however sound
    // the arithmetic.
    for _ in 0..SPLIT_ROUNDS {
        let over = lists.iter().any(|l| l.len() > bounds.max);
        if !over {
            break;
        }
        split_pass(
            corpus,
            &mut lists,
            &mut centroids,
            &mut dirty,
            &mut work,
            bounds,
        );
    }
    // Merge starved lists into their nearest neighbour. Done after splitting so a list that
    // was starved only because its neighbour was about to split is not merged away first.
    let mut keep = vec![true; lists.len()];
    for i in 0..lists.len() {
        if !keep.get(i).copied().unwrap_or(false)
            || lists
                .get(i)
                .is_none_or(|l| l.len() >= bounds.min || l.is_empty())
        {
            continue;
        }
        let Some(cen) = centroids.get(i).cloned() else {
            continue;
        };
        let Some(target) = (0..lists.len())
            .filter(|j| *j != i && keep.get(*j).copied().unwrap_or(false))
            .min_by(|a, b| {
                let da = centroids.get(*a).map_or(f32::INFINITY, |c| dist2(c, &cen));
                let db = centroids.get(*b).map_or(f32::INFINITY, |c| dist2(c, &cen));
                da.total_cmp(&db).then_with(|| a.cmp(b))
            })
        else {
            continue;
        };
        let rows = lists.get(i).cloned().unwrap_or_default();
        if let Some(dst) = lists.get_mut(target) {
            dst.extend(rows);
        }
        if let Some(k) = keep.get_mut(i) {
            *k = false;
        }
        dirty.insert(target);
        work.merges += 1;
    }
    let (mut centroids, mut lists) = (
        centroids
            .into_iter()
            .zip(&keep)
            .filter_map(|(c, k)| k.then_some(c))
            .collect::<Vec<_>>(),
        lists
            .into_iter()
            .zip(&keep)
            .filter_map(|(l, k)| k.then_some(l))
            .collect::<Vec<Vec<usize>>>(),
    );

    // ⚠️ Snapshot BEFORE recentring. A list is disturbed if its centroid moved, and that
    // can only be seen by comparing against where it was — comparing after recentring
    // asks whether recentring would change anything it has already done, which is always
    // no, and silently reduces the protocol to "reassign nothing".
    let before: Vec<Vec<f32>> = centroids.clone();
    for (c, list) in centroids.iter_mut().zip(&lists) {
        recentre(c, corpus, list);
    }

    // ⚠️ **Boundary reassignment, and only that.** A vector is moved when its own centroid
    // is no longer its nearest -- which after a split or merge is a small set near the
    // edited boundaries, not the corpus. `Work::reassigned` reports how small, because
    // "provably a small set in a good index" is a claim about the index, not a guarantee,
    // and a drifted index is exactly when it stops holding.
    // Indices shifted when merged lists were dropped, so a dirty set collected against the
    // old numbering cannot be reused. Recomputed against the surviving lists: a list is
    // dirty if its centroid moved measurably, which is what an edit does and what makes a
    // member's nearest centroid able to change.
    let dirty_now: std::collections::BTreeSet<usize> = match scope {
        Scope::All => (0..lists.len()).collect(),
        Scope::Touched => {
            let mut out = std::collections::BTreeSet::new();
            for (li, c) in centroids.iter().enumerate() {
                // A centroid that barely moved cannot have changed anyone's nearest. The
                // threshold is relative to the list's own spread rather than absolute:
                // recentring after an insert shifts a centroid by ~1/n of a vector, and
                // treating that as a disturbance dirties everything.
                let shifted = before
                    .get(li)
                    .is_none_or(|b| dist2(b, c) > SPLIT_DISTURBANCE);
                if shifted {
                    out.insert(li);
                }
            }
            // Plus their nearest neighbours: a moved centroid can pull vectors from the
            // list next door, which did not move at all.
            let seeds: Vec<usize> = out.iter().copied().collect();
            for i in seeds {
                let Some(ci) = centroids.get(i) else { continue };
                if let Some(j) = (0..centroids.len()).filter(|j| *j != i).min_by(|a, b| {
                    let da = centroids.get(*a).map_or(f32::INFINITY, |c| dist2(c, ci));
                    let db = centroids.get(*b).map_or(f32::INFINITY, |c| dist2(c, ci));
                    da.total_cmp(&db).then_with(|| a.cmp(b))
                }) {
                    out.insert(j);
                }
            }
            out
        }
    };
    // ⚠️ Reassign to a FIXED POINT, not once. Moving a vector moves two centroids, which
    // can make a third vector's nearest change — so a single pass leaves the index still
    // drifting, and the next maintenance cycle finds work on an unchanged corpus. That is
    // an index that rewrites a segment every cycle for nothing. Measured before the fix: a
    // second pass over an untouched corpus still moved 8 rows.
    for _ in 0..REASSIGN_ROUNDS {
        let mut moves: Vec<(usize, usize, usize)> = Vec::new();
        for (li, list) in lists.iter().enumerate() {
            if !dirty_now.contains(&li) {
                continue;
            }
            work.examined += list.len();
            for row in list {
                let Some(v) = corpus.get(*row) else { continue };
                let best = nearest(&centroids, v);
                if best != li {
                    moves.push((*row, li, best));
                }
            }
        }
        if moves.is_empty() {
            break;
        }
        work.reassigned += moves.len();
        for (row, from, to) in moves {
            if let Some(l) = lists.get_mut(from) {
                l.retain(|r| *r != row);
            }
            if let Some(l) = lists.get_mut(to) {
                l.push(row);
            }
        }
        for (c, list) in centroids.iter_mut().zip(&lists) {
            recentre(c, corpus, list);
        }
    }
    for l in lists.iter_mut() {
        l.sort_unstable();
        l.dedup();
    }

    // ⚠️ Drop lists that maintenance emptied. Reassignment can move every row out of a
    // list — an identical-vector corpus does it immediately — and an empty posting list
    // costs a centroid, a directory entry, and a probe that can never return anything.
    // `Clustering::build` already drops them; a maintenance pass that did not would let
    // them accumulate over exactly the cycles this protocol exists to allow.
    let keep: Vec<usize> = (0..lists.len())
        .filter(|i| lists.get(*i).is_some_and(|l| !l.is_empty()))
        .collect();
    let centroids = keep
        .iter()
        .filter_map(|i| centroids.get(*i).cloned())
        .collect();
    let lists = keep.iter().filter_map(|i| lists.get(*i).cloned()).collect();

    *clustering = Clustering::from_parts(centroids, lists);
    work
}

fn nearest(centroids: &[Vec<f32>], v: &[f32]) -> usize {
    centroids
        .iter()
        .enumerate()
        .min_by(|a, b| {
            dist2(a.1, v)
                .total_cmp(&dist2(b.1, v))
                .then_with(|| a.0.cmp(&b.0))
        })
        .map_or(0, |(i, _)| i)
}

fn recentre(c: &mut [f32], corpus: &[Vec<f32>], list: &[usize]) {
    if list.is_empty() {
        return;
    }
    for (d, slot) in c.iter_mut().enumerate() {
        let sum: f32 = list.iter().filter_map(|r| corpus.get(*r)?.get(d)).sum();
        *slot = sum / list.len() as f32;
    }
}

/// Splits one posting list in two by 2-means, seeded by its farthest pair.
///
/// Deterministic: a maintenance pass that varied run to run would make the drift
/// measurement noise rather than evidence.
fn bisect(corpus: &[Vec<f32>], rows: &[usize]) -> (Vec<usize>, Vec<usize>, Vec<f32>, Vec<f32>) {
    let dim = rows
        .first()
        .and_then(|r| corpus.get(*r))
        .map_or(0, Vec::len);
    let Some(first) = rows.first().and_then(|r| corpus.get(*r)) else {
        return (Vec::new(), Vec::new(), vec![0.0; dim], vec![0.0; dim]);
    };
    let far_a = rows
        .iter()
        .max_by(|a, b| {
            let da = corpus.get(**a).map_or(0.0, |v| dist2(v, first));
            let db = corpus.get(**b).map_or(0.0, |v| dist2(v, first));
            da.total_cmp(&db).then_with(|| a.cmp(b))
        })
        .copied()
        .unwrap_or(0);
    let Some(va) = corpus.get(far_a) else {
        return (Vec::new(), Vec::new(), vec![0.0; dim], vec![0.0; dim]);
    };
    let far_b = rows
        .iter()
        .max_by(|a, b| {
            let da = corpus.get(**a).map_or(0.0, |v| dist2(v, va));
            let db = corpus.get(**b).map_or(0.0, |v| dist2(v, va));
            da.total_cmp(&db).then_with(|| a.cmp(b))
        })
        .copied()
        .unwrap_or(0);
    let (mut ca, mut cb) = (
        corpus.get(far_a).cloned().unwrap_or_default(),
        corpus.get(far_b).cloned().unwrap_or_default(),
    );
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..8 {
        a = Vec::new();
        b = Vec::new();
        for row in rows {
            let Some(v) = corpus.get(*row) else { continue };
            if dist2(v, &ca) <= dist2(v, &cb) {
                a.push(*row);
            } else {
                b.push(*row);
            }
        }
        recentre(&mut ca, corpus, &a);
        recentre(&mut cb, corpus, &b);
    }
    (a, b, ca, cb)
}

fn dist2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}
