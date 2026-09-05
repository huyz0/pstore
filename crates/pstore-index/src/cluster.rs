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
}

impl Default for Params {
    fn default() -> Self {
        Self {
            target_list_size: 4_000,
            balance: 4.0,
            iterations: 12,
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
        Self {
            centroids: keep
                .iter()
                .filter_map(|i| centroids.get(*i).cloned())
                .collect(),
            lists: keep.iter().filter_map(|i| lists.get(*i).cloned()).collect(),
        }
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
/// point furthest from every centroid so far. No randomness, so two runs over the same
/// corpus agree exactly — and unlike random sampling it cannot start with two centroids in
/// the same dense blob, which is what leaves a whole region to one list.
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
