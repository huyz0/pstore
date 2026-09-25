//! Balanced clustering — the posting lists a SPANN-family index probes (D-8).
//!
//! ## Why balance is a correctness property, not a tuning knob
//!
//! An unbalanced clustering answers every query and returns plausible results. What it does
//! is make one posting list enormous, so probing it costs a multiple of the byte budget
//! while its neighbours return almost nothing. Recall and cost both degrade, and no
//! functional test sees it — which is why the bound is asserted rather than hoped for.
//!
//! Plain Lloyd's algorithm collapses on exactly the input a real corpus provides: a few
//! dense topics and a long tail. Centroids chase density, so the sparse regions end up
//! sharing one huge list.
//!
//! ## The construction
//!
//! k-means with a **hard capacity**: a vector goes to its nearest centroid that still has
//! room. The cap is what turns "usually balanced" into a bound.
//!
//! ⚠️ An earlier version ordered assignment by *regret* — how much worse a vector's second
//! choice is than its first — on the reasoning that vectors with a strong preference should
//! be placed while there is still room, and the nearly-indifferent ones should absorb the
//! overflow. It sounds right and it is not: measured against the unconstrained k-means
//! objective it bought **nothing**. On interleaved arrival, 1.0591 against row order's
//! 1.0602; on grouped arrival — documents arriving a topic at a time, which is what
//! ingestion actually looks like and the case regret ordering was supposed to rescue —
//! 1.0774 against row order's 1.0756, i.e. slightly *worse*. Removed, and the numbers are
//! recorded here so it is not reinvented.
//!
//! Everything is derived from the data and fixed constants: **no seed, no clock, no
//! iteration-order dependence**, because recall is compared across runs and a clustering
//! that varies makes every comparison noise.

/// How a corpus is divided.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Vectors per posting list, before balance adjustments.
    ///
    /// Sized so a list is one useful ranged GET: too many lists and a probe fetches nothing
    /// useful, too few and it fetches the segment.
    pub target_list_size: usize,
    /// The hard cap, as a multiple of the mean list size.
    pub balance: f32,
    /// Lloyd iterations. Fixed rather than "until convergence": a convergence test makes
    /// the result depend on a tolerance, and the marginal iteration stops mattering long
    /// before it stops running.
    pub iterations: usize,
    /// How many **extra** lists a boundary vector may be replicated into.
    ///
    /// A vector sitting between two centroids is found only if the query probes the list it
    /// happens to have been assigned to. At a small `p` that is a coin flip, and it is
    /// exactly where recall is lost — the true neighbours of a query near a boundary are on
    /// the other side of it. Replication buys them back for index size.
    pub replicas: usize,
    /// How much further than its own centroid a vector may be from another and still be
    /// replicated there, as a fraction.
    ///
    /// 0.0 replicates nothing; 1.0 replicates almost everything and doubles the index.
    pub boundary: f32,
    /// Below this many rows, no index is built and the segment is scanned exactly (D-10).
    ///
    /// A parameter rather than a constant so a test can put the switch within reach of a
    /// small fixture. The **default is the stated number**; a test that lowers it is
    /// testing the switch, not moving it.
    pub exact_scan_threshold: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            target_list_size: 4_000,
            balance: 4.0,
            iterations: 12,
            // ⚠️ Measured, not chosen. 4,000 x 128d clustered, 40 lists, k=10, `rerank:
            // fast` (`cargo run --release -p pstore-index --example aug`):
            //
            //   replicas x boundary   index size   r@10 p=2   r@10 p=8
            //     0 (none)                 1.00      0.844      0.978
            //     1 x 0.05                 1.72      0.936      0.978
            //     1 x 0.10                 1.82      0.961      0.978
            //     2 x 0.10                 2.47      0.978      0.978
            //
            // Three things this settles. Augmentation buys nothing at p=8 -- a wide probe
            // reaches the neighbouring list anyway, so it is purely a small-`p` mechanism.
            // Its cost is steep and non-linear: a second replica nearly doubles the index
            // for a further 1.7 points. And it is worth having anyway, because at equal
            // recall it moves FEWER bytes per query -- p=2 over a 1.72x index reads ~344
            // candidates where p=8 over a 1.0x index reads ~800. Storage is the cheap
            // resource and query bytes are the scarce one, which is the whole "store
            // generously, cache stingily" argument.
            replicas: 1,
            boundary: 0.05,
            exact_scan_threshold: crate::vec_index::EXACT_SCAN_THRESHOLD,
        }
    }
}

/// Centroids and the rows assigned to each.
#[derive(Debug, Clone)]
pub struct Clustering {
    centroids: Vec<Vec<f32>>,
    lists: Vec<Vec<usize>>,
}

impl Clustering {
    /// The centroid of each posting list.
    #[must_use]
    pub fn centroids(&self) -> &[Vec<f32>] {
        &self.centroids
    }

    /// The rows in each posting list, ascending.
    #[must_use]
    pub fn lists(&self) -> &[Vec<usize>] {
        &self.lists
    }

    /// Total squared distance from each vector to the centroid it was assigned.
    ///
    /// The k-means objective, and the only way to tell a *balanced* assignment from a
    /// *good* balanced assignment: both satisfy the capacity bound, and one of them puts
    /// vectors in lists they have no business being in.
    #[must_use]
    pub fn assignment_cost(&self, corpus: &[Vec<f32>]) -> f64 {
        self.lists
            .iter()
            .enumerate()
            .flat_map(|(c, list)| {
                list.iter().filter_map(move |r| {
                    Some(f64::from(dist2(corpus.get(*r)?, self.centroids.get(c)?)))
                })
            })
            .sum()
    }

    /// The same objective if every vector went to its nearest centroid, ignoring capacity.
    ///
    /// A lower bound the balanced assignment cannot beat, so the ratio between them is what
    /// balance actually costs.
    #[must_use]
    pub fn unconstrained_cost(&self, corpus: &[Vec<f32>]) -> f64 {
        corpus
            .iter()
            .map(|v| {
                f64::from(
                    self.centroids
                        .iter()
                        .map(|c| dist2(v, c))
                        .fold(f32::INFINITY, f32::min),
                )
            })
            .sum()
    }

    /// A clustering from parts, for a reader that has centroids but not assignments.
    ///
    /// A searcher holds the centroid table and reads posting lists by byte range; it never
    /// materialises the row lists, so requiring them would force it to fetch what it is
    /// specifically avoiding.
    #[must_use]
    pub fn from_parts(centroids: Vec<Vec<f32>>, lists: Vec<Vec<usize>>) -> Self {
        Self { centroids, lists }
    }

    /// Clusters a corpus.
    #[must_use]
    pub fn build(corpus: &[Vec<f32>], params: Params) -> Self {
        if corpus.is_empty() {
            return Self {
                centroids: Vec::new(),
                lists: Vec::new(),
            };
        }
        let dim = corpus.first().map_or(0, Vec::len);
        let k = corpus.len().div_ceil(params.target_list_size.max(1)).max(1);
        let mut centroids = seed_centroids(corpus, k, dim);
        let cap = capacity(corpus.len(), k, params.balance);

        let mut lists = vec![Vec::new(); k];
        for _ in 0..params.iterations.max(1) {
            lists = assign(corpus, &centroids, cap);
            // Recompute each centroid as the mean of what it actually holds. A centroid
            // that drifts from its members makes probe selection pick the wrong lists, and
            // recall falls for a reason nothing in the search path can explain.
            for (c, list) in centroids.iter_mut().zip(&lists) {
                if list.is_empty() {
                    continue;
                }
                for (d, slot) in c.iter_mut().enumerate() {
                    let sum: f32 = list.iter().filter_map(|r| corpus.get(*r)?.get(d)).sum();
                    *slot = sum / list.len() as f32;
                }
            }
        }

        // An empty list costs a centroid, a directory entry, and a probe that can never
        // return anything.
        let keep: Vec<usize> = (0..k)
            .filter(|i| lists.get(*i).is_some_and(|l| !l.is_empty()))
            .collect();
        let centroids: Vec<Vec<f32>> = keep
            .iter()
            .filter_map(|i| centroids.get(*i).cloned())
            .collect();
        let mut lists: Vec<Vec<usize>> =
            keep.iter().filter_map(|i| lists.get(*i).cloned()).collect();

        // ⚠️ Replication happens AFTER empty lists are dropped, so a boundary vector is
        // never replicated into a list that is about to disappear.
        augment(corpus, &centroids, &mut lists, params);
        Self { centroids, lists }
    }
}

fn capacity(n: usize, k: usize, balance: f32) -> usize {
    let mean = n as f32 / k as f32;
    // ⚠️ Rounded DOWN against the bound, so the cap is inside it rather than on it. A cap
    // computed as exactly `balance * mean` and then rounded up produces lists one element
    // over the bound the tests assert, which reads as an off-by-one in the test rather than
    // in the cap.
    ((mean * balance).floor() as usize).max(1)
}

/// Squared L2. The root is monotonic in the same order and costs an operation per vector on
/// a path that runs `n * k` times.
fn dist2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// Initial centroids, spread rather than sampled.
///
/// Farthest-point traversal from a deterministic start: each new centroid is the corpus
/// point furthest from every centroid so far, ties to the lowest row. No randomness, so two
/// runs over the same corpus agree exactly — and unlike random sampling it cannot start with
/// two centroids in the same dense blob, which is what leaves a whole region to one list.
fn seed_centroids(corpus: &[Vec<f32>], k: usize, dim: usize) -> Vec<Vec<f32>> {
    let mut chosen: Vec<Vec<f32>> = Vec::with_capacity(k);
    chosen.push(corpus.first().cloned().unwrap_or_else(|| vec![0.0; dim]));
    let mut best: Vec<f32> = corpus
        .iter()
        .map(|v| dist2(v, chosen.first().map_or(&[][..], Vec::as_slice)))
        .collect();
    while chosen.len() < k {
        let (far, _) = best
            .iter()
            .enumerate()
            .fold((0usize, f32::NEG_INFINITY), |acc, (i, d)| {
                if *d > acc.1 { (i, *d) } else { acc }
            });
        let Some(next) = corpus.get(far).cloned() else {
            break;
        };
        for (b, v) in best.iter_mut().zip(corpus) {
            *b = b.min(dist2(v, &next));
        }
        chosen.push(next);
    }
    chosen
}

/// Replicates boundary vectors into nearby lists.
///
/// ⚠️ **This is where recall at small `p` comes from.** A vector midway between two
/// centroids belongs, as far as any query is concerned, to both: a query near the boundary
/// probes one list and the answer is in the other. Assignment has to pick one; replication
/// undoes the arbitrariness for the vectors where it matters, and only for those.
///
/// Bounded twice — by `replicas` and by `boundary` — because the degenerate version
/// replicates everything into everything and calls the resulting exhaustive scan a
/// clustered index.
fn augment(corpus: &[Vec<f32>], centroids: &[Vec<f32>], lists: &mut [Vec<usize>], params: Params) {
    if params.replicas == 0 || params.boundary <= 0.0 || centroids.len() < 2 {
        return;
    }
    // Pass one: every replica the boundary rule would admit, with its distance, so pass two
    // can prefer the closest when a list runs out of room.
    let mut extra: Vec<Vec<(usize, f32)>> = vec![Vec::new(); lists.len()];
    for (row, v) in corpus.iter().enumerate() {
        let mut d: Vec<(usize, f32)> = centroids
            .iter()
            .enumerate()
            .map(|(c, cen)| (c, dist2(v, cen)))
            .collect();
        d.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let Some((_, nearest)) = d.first().copied() else {
            continue;
        };
        // Compared in true distance, not squared: `boundary` is a fraction of a distance,
        // and applying it to a squared value would make the threshold quietly
        // dimension-dependent.
        let limit = nearest.sqrt() * (1.0 + params.boundary);
        for (c, dd) in d.iter().skip(1).take(params.replicas) {
            if dd.sqrt() <= limit
                && let Some(slot) = extra.get_mut(*c)
            {
                slot.push((row, *dd));
            }
        }
    }

    // ⚠️ Pass two enforces the SAME balance bound on the augmented lists. Replication that
    // ignores the cap undoes it entirely: measured on 10:1 skewed data, unbounded
    // replication took the largest list to 6.0x the mean, which is exactly the cost the cap
    // exists to prevent — and a probe reads the augmented list, not the assigned one.
    //
    // The cap is computed against the mean *after* replication, so augmentation is allowed
    // to grow every list proportionally; what it may not do is grow one of them.
    let admitted: usize = extra.iter().map(Vec::len).sum();
    let total = corpus.len().saturating_add(admitted);
    let cap = capacity(total, lists.len().max(1), params.balance);
    for (list, mut add) in lists.iter_mut().zip(extra) {
        // Closest first, so a list that fills keeps the replicas that mattered most.
        add.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        for (row, _) in add {
            if list.len() >= cap {
                break;
            }
            list.push(row);
        }
        list.sort_unstable();
        // A row replicated into a list it was also assigned to would be scored twice and
        // could occupy two slots in a top-k.
        list.dedup();
    }
}

/// Assigns every row to its nearest centroid that still has room.
fn assign(corpus: &[Vec<f32>], centroids: &[Vec<f32>], cap: usize) -> Vec<Vec<usize>> {
    let mut lists: Vec<Vec<usize>> = vec![Vec::new(); centroids.len()];
    for (row, v) in corpus.iter().enumerate() {
        let mut prefs: Vec<(usize, f32)> = centroids
            .iter()
            .enumerate()
            .map(|(c, cen)| (c, dist2(v, cen)))
            .collect();
        // Ties by centroid index, so the order is total and two runs over the same corpus
        // agree exactly. Recall is compared across runs; a clustering that varies makes
        // every comparison noise.
        prefs.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        // Nearest centroid with room. Every vector lands somewhere: the fallback is the
        // least-full list, so a corpus can never lose a row to a full-everywhere state --
        // and a lost row is unreachable at any probe width, visible only as a recall
        // ceiling.
        let target = prefs
            .iter()
            .find(|(c, _)| lists.get(*c).is_some_and(|l| l.len() < cap))
            .map(|(c, _)| *c)
            .unwrap_or_else(|| {
                lists
                    .iter()
                    .enumerate()
                    .min_by_key(|(i, l)| (l.len(), *i))
                    .map_or(0, |(i, _)| i)
            });
        if let Some(list) = lists.get_mut(target) {
            list.push(row);
        }
    }
    lists
}
