//! M162: an index measures its own recall, online, at one snapshot.
//!
//! Sampled queries are the vectors of the documents nearest one probe drawn from a seed, so
//! the same seed over the same snapshot measures the same queries. Each is searched twice
//! against the one HEAD the request read: as a default query searches, and exhaustively.

use crate::{Engine, EngineError, head, quarantine_exists, replica, schema_recorded_at};
use pstore_blob::BlobStore;
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_types::Epoch;
use std::collections::HashSet;

/// Row-scores one recall request may cost (M162): each search that scores every row of the
/// snapshot counts its rows. Refused past it, from HEAD alone, before any segment read.
pub const RECALL_ROW_BUDGET: u64 = 2_000_000;

/// What to measure (M162). The server bounds `num` to 1..=25 and `top_k` to 1..=100.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallRequest {
    /// Queries to sample.
    pub num: usize,
    /// Results compared a query.
    pub top_k: usize,
    /// Applied to both searches of every sampled query, never to the sampling.
    ///
    /// ⚠️ A filter makes every dense leg probe every list (M9b), so a filtered recall
    /// measures the compressed scoring only, never a missed list.
    pub filter: Option<pstore_query::Predicate>,
    /// Draws the probe the queries are sampled near.
    pub seed: u64,
    /// Whether to return each query with its exhaustive neighbours.
    pub ground_truth: bool,
}

/// What a recall request measured (M162).
#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    /// The HEAD every search answered at.
    pub epoch: Epoch,
    /// The queries sampled: `num`, or fewer when the index holds fewer documents.
    pub num: usize,
    /// The mean, over the queries whose exhaustive search returned anything, of the share of
    /// its ids the approximate search also returned. `None` when no query has one.
    pub avg_recall: Option<f64>,
    /// The mean count of approximate results.
    pub avg_ann_count: f64,
    /// The mean count of exhaustive results.
    pub avg_exhaustive_count: f64,
    /// Each sampled query and its exhaustive results, when asked for; else empty.
    pub ground_truth: Vec<GroundTruth>,
}

/// One sampled query and its exhaustive results (M162).
#[derive(Debug, Clone, PartialEq)]
pub struct GroundTruth {
    /// The sampled document's vector in client space (normalized under cosine).
    pub query_vector: Vec<f32>,
    /// The exhaustive results in rank order, with the distance a query reports.
    pub nearest_neighbors: Vec<(String, f32)>,
}

impl<S: BlobStore> Engine<S> {
    /// Measures `index`'s recall at the HEAD it reads once (M162). `None` for an index that
    /// HEAD does not hold.
    ///
    /// # Errors
    /// [`EngineError::RecallTooLarge`] past [`RECALL_ROW_BUDGET`],
    /// [`EngineError::RecallNeedsVectors`] for an index without default vectors, a replica's
    /// refusal, or as a query fails.
    pub async fn recall(
        &self,
        index: &str,
        req: &RecallRequest,
    ) -> Result<Option<Recall>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        if at.head.replications.contains_key(index) {
            return Err(EngineError::Replica(replica::ReplicaRefusal::NoHistory(
                index.to_owned(),
            )));
        }
        if !quarantine_exists(&at.head, index) {
            return Ok(None);
        }
        let epoch = at.head.epoch;
        let recorded = schema_recorded_at(&at.head, index, epoch);
        let dims = recorded.as_ref().map_or(0, |s| s.client_dims() as usize);
        if dims == 0 {
            return Err(EngineError::RecallNeedsVectors(index.to_owned()));
        }
        // Every search that scores every row counts the snapshot's rows: each exhaustive one,
        // and with a filter each approximate one too (M9b probes every list).
        let rows: u64 = at
            .head
            .indexes
            .get(index)
            .into_iter()
            .flatten()
            .map(|r| u64::from(r.rows))
            .sum();
        within_budget(rows, req.num, req.filter.is_some())?;
        let search = |q: Vec<f32>, k: usize, exact: bool, filter, vectors| {
            let schema = recorded.clone();
            let at = &at;
            async move {
                self.query_head(
                    &at.head,
                    schema,
                    index,
                    &[dense(q, k, exact)],
                    filter,
                    Fusion::default(),
                    k,
                    vectors,
                )
                .await
            }
        };
        let near = search(recall_probe(req.seed, dims), req.num, false, None, true).await?;
        let queries: Vec<Vec<f32>> = near.vectors.into_iter().flatten().collect();
        let filter = req.filter.as_ref();
        let both = futures_util::future::try_join_all(queries.iter().map(|q| {
            futures_util::future::try_join(
                search(q.clone(), req.top_k, false, filter, false),
                search(q.clone(), req.top_k, true, filter, false),
            )
        }))
        .await?;
        let (mut shares, mut ann, mut exhaustive) = (Vec::new(), 0usize, 0usize);
        let mut ground_truth = Vec::new();
        for (q, (a, x)) in queries.iter().zip(both) {
            let found: HashSet<&String> = a.ids.iter().flatten().collect();
            let truth: Vec<(String, f32)> = x
                .ids
                .into_iter()
                .zip(x.dists)
                .filter_map(|(id, d)| Some((id?, d.unwrap_or(f32::NAN))))
                .collect();
            ann += found.len();
            exhaustive += truth.len();
            if !truth.is_empty() {
                let hit = truth.iter().filter(|(id, _)| found.contains(id)).count();
                shares.push(hit as f64 / truth.len() as f64);
            }
            if req.ground_truth {
                ground_truth.push(GroundTruth {
                    query_vector: q.clone(),
                    nearest_neighbors: truth,
                });
            }
        }
        Ok(Some(Recall {
            epoch,
            num: queries.len(),
            avg_recall: (!shares.is_empty()).then(|| mean(shares.iter().sum(), shares.len())),
            avg_ann_count: mean(ann as f64, queries.len()),
            avg_exhaustive_count: mean(exhaustive as f64, queries.len()),
            ground_truth,
        }))
    }
}

/// Refuses a request whose searches would score more than [`RECALL_ROW_BUDGET`] rows:
/// `rows` for each exhaustive search, and with a filter for each approximate one too.
fn within_budget(rows: u64, num: usize, filtered: bool) -> Result<(), EngineError> {
    let per_query = if filtered { 2 } else { 1 };
    let scored = rows.saturating_mul(per_query * num as u64);
    if scored > RECALL_ROW_BUDGET {
        return Err(EngineError::RecallTooLarge {
            rows: scored,
            budget: RECALL_ROW_BUDGET,
        });
    }
    Ok(())
}

/// `sum / n`, and 0 over nothing.
fn mean(sum: f64, n: usize) -> f64 {
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// A default dense leg over the default field, or an exhaustive one.
fn dense(query: Vec<f32>, limit: usize, exact: bool) -> Prefetch {
    Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query,
        limit,
        tune: Query {
            exact,
            ..Query::default()
        },
    }
}

/// The probe `seed` draws (M162): `dims` coordinates, each uniform in [-1, 1), from
/// splitmix64. A recall request samples the documents an approximate search for it returns.
#[must_use]
pub fn recall_probe(seed: u64, dims: usize) -> Vec<f32> {
    let mut state = seed;
    (0..dims)
        .map(|_| {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            f32::from(u16::try_from(z >> 48).unwrap_or(0)) / 32768.0 - 1.0
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_is_its_seeds() {
        let a = recall_probe(1, 64);
        assert_eq!(a, recall_probe(1, 64));
        assert_ne!(a, recall_probe(2, 64));
        assert_eq!(a.len(), 64);
        assert!(a.iter().all(|x| (-1.0..1.0).contains(x)), "{a:?}");
        assert!(
            a.iter().any(|x| *x < -0.5) && a.iter().any(|x| *x > 0.5),
            "{a:?}"
        );
        let zero = recall_probe(0, 4);
        // splitmix64's first three outputs from seed 0 are 0xE220A8397B1DCDAF,
        // 0x6E789E6AA1B965F4 and 0x06C45D188009454F: their top 16 bits, scaled.
        let want = [0xE220, 0x6E78, 0x06C4].map(|top: u16| f32::from(top) / 32768.0 - 1.0);
        assert_eq!(zero.get(..3), Some(&want[..]), "{zero:?}");
        assert!(zero.windows(2).any(|w| w.first() != w.last()), "{zero:?}");
    }

    #[test]
    fn the_budget_is_inclusive_and_doubles_with_a_filter() {
        assert!(within_budget(RECALL_ROW_BUDGET, 1, false).is_ok());
        assert!(within_budget(RECALL_ROW_BUDGET + 1, 1, false).is_err());
        assert!(within_budget(RECALL_ROW_BUDGET / 2, 2, false).is_ok());
        assert!(within_budget(RECALL_ROW_BUDGET / 2, 1, true).is_ok());
        assert!(within_budget(RECALL_ROW_BUDGET / 2 + 1, 1, true).is_err());
        assert!(within_budget(u64::MAX, 25, true).is_err());
    }

    #[test]
    fn a_mean_over_nothing_is_zero() {
        assert!(mean(3.0, 0).abs() < f64::EPSILON);
        assert!((mean(3.0, 2) - 1.5).abs() < f64::EPSILON);
    }
}
