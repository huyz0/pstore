//! Outer RRF over a multi-query (M142): each query's answer, as returned, fused into one
//! ranking by reciprocal rank, then windowed by `offset` and `limit`.
//!
//! ⚠️ **`f64`, added in query order, cast to `f32` only in the answer** (spec review):
//! floating-point addition is not associative, so a sum in another order or precision can
//! split two mathematically equal scores by one ULP, and their order would then be an
//! accident of the arithmetic rather than the id's.

use crate::types::ResultRow;
use serde_json::Value;
use std::collections::HashMap;

/// `rank_constant` when the request names none: RRF's classic 60.
const DEFAULT_RANK_CONSTANT: u64 = 60;
/// The largest weight a query may carry: past it an `f32` answer could overflow.
const MAX_WEIGHT: f64 = 1_000_000.0;

/// A multi-query's outer fusion: its `rerank_by`, `limit` and `offset`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outer {
    rank_constant: f64,
    /// One per query, in query order.
    weights: Vec<f64>,
    offset: usize,
    limit: Option<usize>,
}

impl Outer {
    /// The outer fusion of a multi-query of `queries` queries.
    ///
    /// # Errors
    /// Why the three fields cannot mean an outer RRF, as a refusal's message.
    pub(crate) fn parse(
        rerank_by: &Value,
        limit: Option<&Value>,
        offset: Option<&Value>,
        queries: usize,
    ) -> Result<Self, String> {
        let malformed = || {
            "rerank_by is [\"RRF\"] or [\"RRF\", {\"rank_constant\": n, \"weights\": [...]}]; \
             model reranking is not offered"
                .to_owned()
        };
        let params = match rerank_by.as_array().map(Vec::as_slice) {
            Some([Value::String(f)]) if f == "RRF" => None,
            Some([Value::String(f), Value::Object(p)]) if f == "RRF" => Some(p),
            _ => return Err(malformed()),
        };
        let mut rank_constant = DEFAULT_RANK_CONSTANT;
        let mut weights = vec![1.0; queries];
        for (key, value) in params.into_iter().flatten() {
            match key.as_str() {
                "rank_constant" => {
                    rank_constant = value.as_u64().filter(|k| *k >= 1).ok_or_else(|| {
                        "rerank_by's rank_constant is an integer of at least 1".to_owned()
                    })?;
                }
                "weights" => weights = weights_of(value, queries)?,
                other => return Err(format!("rerank_by has no parameter {other:?}")),
            }
        }
        let limit = limit
            .map(|l| {
                match l {
                    Value::Object(o) if o.len() == 1 => o.get("total").and_then(Value::as_u64),
                    l => l.as_u64(),
                }
                .ok_or_else(|| "limit is an integer of at least 0, or {\"total\": n}".to_owned())
            })
            .transpose()?;
        let offset = offset
            .map(|o| {
                o.as_u64()
                    .ok_or_else(|| "offset is an integer of at least 0".to_owned())
            })
            .transpose()?
            .unwrap_or(0);
        Ok(Self {
            #[allow(
                clippy::cast_precision_loss,
                reason = "a constant past 2^53 ranks alike"
            )]
            rank_constant: rank_constant as f64,
            weights,
            offset: usize::try_from(offset).unwrap_or(usize::MAX),
            limit: limit.map(|l| usize::try_from(l).unwrap_or(usize::MAX)),
        })
    }
}

/// `weights`: one per query, each in (0, [`MAX_WEIGHT`]].
fn weights_of(value: &Value, queries: usize) -> Result<Vec<f64>, String> {
    let wrong = || {
        format!(
            "rerank_by's weights are {queries} numbers, one per query, each greater than 0 and \
             at most {MAX_WEIGHT}"
        )
    };
    let weights = value.as_array().ok_or_else(wrong)?;
    if weights.len() != queries {
        return Err(wrong());
    }
    weights
        .iter()
        .map(|w| {
            w.as_f64()
                .filter(|w| *w > 0.0 && *w <= MAX_WEIGHT)
                .ok_or_else(wrong)
        })
        .collect()
}

/// Each query's answer, in query order, fused into one ranking and windowed.
///
/// A row's score is the sum of `weight / (rank_constant + rank)` over the answers holding
/// it, its rank counted from 1; its attributes are the first answer's that carries any.
/// Ordered by score, descending, then id, ascending: no answer's order decides a tie.
pub(crate) fn fuse_outer(lists: Vec<Vec<ResultRow>>, outer: &Outer) -> Vec<ResultRow> {
    let mut fused: Vec<(f64, ResultRow)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (list, weight) in lists.into_iter().zip(&outer.weights) {
        for (rank, row) in (1u32..).zip(list) {
            let share = weight / (outer.rank_constant + f64::from(rank));
            match at.get(&row.id) {
                Some(&i) => {
                    if let Some((score, kept)) = fused.get_mut(i) {
                        *score += share;
                        if kept.attributes.is_none() {
                            kept.attributes = row.attributes;
                        }
                    }
                }
                None => {
                    at.insert(row.id.clone(), fused.len());
                    fused.push((
                        share,
                        ResultRow {
                            id: row.id,
                            score: None,
                            dist: None,
                            attributes: row.attributes,
                            highlight: None,
                            highlights: None,
                            vector: None,
                            vectors: None,
                        },
                    ));
                }
            }
        }
    }
    fused.sort_by(|(a, x), (b, y)| {
        b.total_cmp(a)
            .then_with(|| x.id.as_bytes().cmp(y.id.as_bytes()))
    });
    fused
        .into_iter()
        .skip(outer.offset)
        .take(outer.limit.unwrap_or(usize::MAX))
        .map(|(score, mut row)| {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the answer's score is an f32"
            )]
            let score = score as f32;
            row.score = Some(score);
            row
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};

    fn row(id: &str, attr: Option<i64>) -> ResultRow {
        ResultRow {
            id: id.to_owned(),
            score: Some(0.5),
            dist: Some(0.25),
            attributes: attr.map(|a| BTreeMap::from([("a".to_owned(), json!(a))])),
            highlight: Some("h".to_owned()),
            highlights: None,
            vector: None,
            vectors: None,
        }
    }

    fn outer(
        params: serde_json::Value,
        limit: Option<u64>,
        offset: Option<u64>,
        n: usize,
    ) -> Outer {
        Outer::parse(
            &json!(["RRF", params]),
            limit.map(|l| json!(l)).as_ref(),
            offset.map(|o| json!(o)).as_ref(),
            n,
        )
        .unwrap()
    }

    /// The reference: written apart from `fuse_outer`, from the spec's words alone.
    fn reference(
        lists: &[Vec<ResultRow>],
        k: f64,
        weights: &[f64],
        offset: usize,
        limit: usize,
    ) -> Vec<(String, f32)> {
        let mut score: HashMap<String, f64> = HashMap::new();
        for (q, list) in lists.iter().enumerate() {
            for (i, r) in list.iter().enumerate() {
                *score.entry(r.id.clone()).or_insert(0.0) += weights[q] / (k + (i + 1) as f64);
            }
        }
        let mut all: Vec<(String, f64)> = score.into_iter().collect();
        all.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
        });
        all.into_iter()
            .skip(offset)
            .take(limit)
            .map(|(id, s)| (id, s as f32))
            .collect()
    }

    #[test]
    fn fusion_equals_a_reference_over_random_rankings() {
        let mut state = 0x853C_49E6_748F_EA9B_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..300 {
            let n = (next() % 16 + 1) as usize;
            let lists: Vec<Vec<ResultRow>> = (0..n)
                .map(|_| {
                    let len = next() % 12;
                    let mut ids: Vec<String> = Vec::new();
                    while (ids.len() as u64) < len {
                        let id = format!("d{}", next() % 20);
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                    ids.iter().map(|id| row(id, None)).collect()
                })
                .collect();
            // Small integer weights and constants, so many fused scores tie.
            let k = (next() % 5 + 1) as f64;
            let weights: Vec<f64> = (0..n).map(|_| (next() % 3 + 1) as f64).collect();
            let offset = next() % 6;
            let limit = next() % 25;
            let o = outer(
                json!({"rank_constant": k as u64, "weights": weights}),
                Some(limit),
                Some(offset),
                n,
            );
            let got: Vec<(String, f32)> = fuse_outer(lists.clone(), &o)
                .into_iter()
                .map(|r| (r.id, r.score.unwrap()))
                .collect();
            let want = reference(&lists, k, &weights, offset as usize, limit as usize);
            assert_eq!(
                got, want,
                "k {k}, weights {weights:?}, offset {offset}, limit {limit}"
            );
        }
    }

    #[test]
    fn defaults_are_sixty_weights_of_one_and_every_row() {
        let lists = vec![
            vec![row("a", None), row("b", None)],
            vec![row("b", None), row("c", None)],
        ];
        let o = Outer::parse(&json!(["RRF"]), None, None, 2).unwrap();
        let got = fuse_outer(lists.clone(), &o);
        let want = reference(&lists, 60.0, &[1.0, 1.0], 0, usize::MAX);
        let got: Vec<(String, f32)> = got.into_iter().map(|r| (r.id, r.score.unwrap())).collect();
        assert_eq!(got, want);
        assert_eq!(got[0].0, "b", "returned twice, fused once");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn a_row_carries_the_first_attributes_and_no_dist_or_highlight() {
        let lists = vec![
            vec![row("x", None)],
            vec![row("x", Some(2))],
            vec![row("x", Some(3))],
        ];
        let o = Outer::parse(&json!(["RRF"]), None, None, 3).unwrap();
        let got = fuse_outer(lists, &o);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].attributes.as_ref().unwrap()["a"], json!(2));
        assert_eq!(got[0].dist, None);
        assert_eq!(got[0].highlight, None);
    }

    #[test]
    fn equal_scores_order_by_id_whatever_the_query_order() {
        // One contribution each, and two added in either order: exact ties by construction.
        let one = vec![row("q", None), row("p", None)];
        let two = vec![row("p", None), row("q", None)];
        let solo_a = vec![row("z", None)];
        let solo_b = vec![row("y", None)];
        for lists in [
            vec![one.clone(), two.clone(), solo_a.clone(), solo_b.clone()],
            vec![solo_b, two, solo_a, one],
        ] {
            let o = Outer::parse(&json!(["RRF"]), None, None, 4).unwrap();
            let ids: Vec<String> = fuse_outer(lists, &o).into_iter().map(|r| r.id).collect();
            assert_eq!(ids, ["p", "q", "y", "z"]);
        }
    }

    #[test]
    fn offset_is_applied_before_limit() {
        let lists = vec![(0..6).map(|i| row(&format!("r{i}"), None)).collect()];
        let o = outer(json!({}), Some(2), Some(3), 1);
        let ids: Vec<String> = fuse_outer(lists, &o).into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["r3", "r4"]);
    }

    #[test]
    fn a_limit_object_means_its_total() {
        let lists = vec![
            (0..6)
                .map(|i| row(&format!("r{i}"), None))
                .collect::<Vec<_>>(),
        ];
        let o = Outer::parse(&json!(["RRF"]), Some(&json!({"total": 2})), None, 1).unwrap();
        assert_eq!(fuse_outer(lists, &o).len(), 2);
    }

    #[test]
    fn what_cannot_mean_an_outer_rrf_is_refused() {
        let parse =
            |r: serde_json::Value, l: Option<serde_json::Value>, o: Option<serde_json::Value>| {
                Outer::parse(&r, l.as_ref(), o.as_ref(), 2)
            };
        for (r, l, o) in [
            (json!("RRF"), None, None),
            (json!([]), None, None),
            (json!(["rrf"]), None, None),
            (json!(["RRF", {}, {}]), None, None),
            (json!(["RRF", []]), None, None),
            (
                json!({"model": "m", "attribute": "a", "query": "q"}),
                None,
                None,
            ),
            (json!(["RRF", {"rank_constant": 0}]), None, None),
            (json!(["RRF", {"rank_constant": 60.0}]), None, None),
            (json!(["RRF", {"rank_constant": -1}]), None, None),
            (json!(["RRF", {"weights": [1.0]}]), None, None),
            (json!(["RRF", {"weights": [1.0, 1.0, 1.0]}]), None, None),
            (json!(["RRF", {"weights": [1.0, 0.0]}]), None, None),
            (json!(["RRF", {"weights": [1.0, -2.0]}]), None, None),
            (json!(["RRF", {"weights": [1.0, 1_000_001.0]}]), None, None),
            (json!(["RRF", {"weights": [1.0, "1"]}]), None, None),
            (json!(["RRF", {"k": 60}]), None, None),
            (json!(["RRF"]), Some(json!(-1)), None),
            (json!(["RRF"]), Some(json!(2.0)), None),
            (json!(["RRF"]), Some(json!({})), None),
            (json!(["RRF"]), Some(json!({"total": 2, "x": 1})), None),
            (json!(["RRF"]), Some(json!({"total": -2})), None),
            (json!(["RRF"]), None, Some(json!(-1))),
            (json!(["RRF"]), None, Some(json!(1.5))),
        ] {
            assert!(
                parse(r.clone(), l.clone(), o.clone()).is_err(),
                "{r} {l:?} {o:?}"
            );
        }
        // The bounds themselves are allowed.
        assert!(
            parse(
                json!(["RRF", {"rank_constant": 1, "weights": [1_000_000.0, 1e-9]}]),
                Some(json!(0)),
                Some(json!(0))
            )
            .is_ok()
        );
    }
}
