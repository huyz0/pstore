//! Probe selection and the candidate set — the search half of a SPANN-family index.
//!
//! ⚠️ **This module does no I/O.** It decides *which posting lists to read* and scores what
//! it is handed. That separation is what makes recall testable against brute force with no
//! store in the loop, and it is also the shape the round-trip budget requires: the probe
//! set is known before any fetch is issued, so all `p` lists go out together. A search that
//! chose its next list from the contents of the last would be a data-dependent chain, which
//! is the one thing the architecture forbids.

use crate::cluster::Clustering;

/// Which lists a query should read.
///
/// `p` is free in **depth** and costs **bytes**: 64 lists is the same one round trip as 8,
/// and eight times the transfer. That asymmetry is the lever a memory-resident index does
/// not have, and it is why `p` is the recall knob rather than a graph traversal depth.
#[must_use]
pub fn probe(clustering: &Clustering, query: &[f32], p: usize) -> Vec<usize> {
    let mut d: Vec<(usize, f32)> = clustering
        .centroids()
        .iter()
        .enumerate()
        .map(|(i, c)| (i, dist2(query, c)))
        .collect();
    // Ties by index so a probe set is reproducible; recall is compared across runs.
    d.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    d.truncate(p.max(1));
    d.into_iter().map(|(i, _)| i).collect()
}

/// Which lists a query should read, dropping those too far to contribute.
///
/// ⚠️ Query-aware pruning: a query sitting **on** a centroid has one obviously-right list
/// and the rest are noise, while a query equidistant between several genuinely needs them
/// all. Probing a fixed `p` either over-fetches for the easy query or under-fetches for the
/// hard one. `slack` is how much further than the nearest centroid a list may be and still
/// be worth its bytes.
#[must_use]
pub fn probe_pruned(clustering: &Clustering, query: &[f32], p: usize, slack: f32) -> Vec<usize> {
    let chosen = probe(clustering, query, p);
    let Some(nearest) = chosen
        .first()
        .and_then(|i| clustering.centroids().get(*i))
        .map(|c| dist2(query, c).sqrt())
    else {
        return chosen;
    };
    // True distance, not squared: `slack` is a fraction of a distance, and applying it to a
    // squared value makes the threshold quietly dimension-dependent.
    let limit = nearest * (1.0 + slack);
    let kept: Vec<usize> = chosen
        .iter()
        .copied()
        .filter(|i| {
            clustering
                .centroids()
                .get(*i)
                .is_some_and(|c| dist2(query, c).sqrt() <= limit)
        })
        .collect();
    // Never empty: pruning that can return nothing turns a hard query into no answer at
    // all, which is a worse failure than fetching a list that did not help.
    if kept.is_empty() { chosen } else { kept }
}

/// Every row reachable through `lists`, deduplicated.
///
/// Boundary replication means a row can appear in several probed lists; scoring it twice
/// would let one vector occupy two slots of a top-k.
#[must_use]
pub fn candidates(clustering: &Clustering, lists: &[usize]) -> Vec<usize> {
    let mut out: Vec<usize> = lists
        .iter()
        .filter_map(|i| clustering.lists().get(*i))
        .flatten()
        .copied()
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn dist2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}
