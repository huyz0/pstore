//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Distance metrics in the engine — M9d: a clustered segment under each metric, the fold's
//! metric check, and what `exact` costs.

use pstore_blob::MemoryStore;
use pstore_engine::{Engine, Metric};
use pstore_format::Document;
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

const DIM: usize = 8;
const ROWS: usize = 600;

/// Clustered data whose norms spread over 10x, deterministic.
fn corpus(n: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let centres: Vec<Vec<f32>> = (0..12)
        .map(|_| (0..DIM).map(|_| next() * 4.0).collect())
        .collect();
    (0..n)
        .map(|i| {
            let c = &centres[i % centres.len()];
            let scale = 1.0 + 9.0 * ((i * 7919) % n) as f32 / n as f32;
            c.iter().map(|x| (x + next() * 0.6) * scale).collect()
        })
        .collect()
}

fn brute(metric: Metric, q: &[f32], v: &[f32]) -> f32 {
    let dot: f32 = q.iter().zip(v).map(|(a, b)| a * b).sum();
    let norm = |x: &[f32]| x.iter().map(|a| a * a).sum::<f32>().sqrt();
    match metric {
        Metric::CosineDistance => 1.0 - dot / (norm(q) * norm(v)),
        Metric::EuclideanSquared => q.iter().zip(v).map(|(a, b)| (a - b) * (a - b)).sum(),
        Metric::DotProduct => -dot,
    }
}

fn dense(q: &[f32], limit: usize, exact: bool) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: q.to_vec(),
        limit,
        tune: Query {
            exact,
            ..Query::default()
        },
    }]
}

async fn clustered<S: pstore_blob::BlobStore>(
    store: Arc<S>,
    metric: Metric,
    docs: &[Vec<f32>],
) -> Engine<S> {
    let e = Engine::new(store, TenantId(31), LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            target_list_size: 20,
            exact_scan_threshold: 100,
            ..pstore_index::cluster::Params::default()
        },
    );
    let rows: Vec<Document> = docs
        .iter()
        .enumerate()
        .map(|(i, v)| Document::new(format!("d{i:04}"), v.clone()))
        .collect();
    e.write_as("idx", rows, metric).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e
}

async fn top<S: pstore_blob::BlobStore>(
    e: &Engine<S>,
    q: &[f32],
    k: usize,
    exact: bool,
) -> BTreeSet<String> {
    let answer = e
        .query("idx", &dense(q, k, exact), Fusion::Rrf { k: 60.0 }, k)
        .await
        .unwrap();
    e.resolve(&answer).into_iter().map(|(id, _)| id).collect()
}

#[tokio::test]
async fn a_clustered_segment_recalls_under_every_metric() {
    let docs = corpus(ROWS, 0x2545_F491_4F6C_DD1D);
    let queries = corpus(20, 0xA5A5_5A5A_1234_5678);
    for metric in [Metric::CosineDistance, Metric::EuclideanSquared] {
        let e = clustered(Arc::new(MemoryStore::new()), metric, &docs).await;
        // Premise: the probe is not exhaustive -- asking for every row misses some.
        let all = top(&e, &queries[0], ROWS, false).await;
        assert!(all.len() < ROWS, "{metric:?}: the probe read every list");
        let mut found = 0usize;
        for q in &queries {
            let mut truth: Vec<(f32, usize)> = docs
                .iter()
                .enumerate()
                .map(|(i, v)| (brute(metric, q, v), i))
                .collect();
            truth.sort_by(|a, b| a.0.total_cmp(&b.0));
            let want: BTreeSet<String> = truth[..10]
                .iter()
                .map(|(_, i)| format!("d{i:04}"))
                .collect();
            found += top(&e, q, 10, false).await.intersection(&want).count();
            // And `exact` is exact.
            assert_eq!(top(&e, q, 10, true).await, want, "{metric:?}: exact missed");
        }
        let recall = found as f32 / (10.0 * queries.len() as f32);
        assert!(recall >= 0.8, "{metric:?}: recall@10 {recall}");
    }
}

#[tokio::test]
async fn exact_adds_no_round_trip() {
    let docs = corpus(ROWS, 7);
    let q = corpus(1, 9).remove(0);
    for metric in [Metric::DotProduct, Metric::EuclideanSquared] {
        let store = Arc::new(DepthCounting::new(MemoryStore::new()));
        let e = clustered(Arc::clone(&store), metric, &docs).await;
        store.reset();
        top(&e, &q, 10, false).await;
        let approximate = store.depth();
        store.reset();
        top(&e, &q, 10, true).await;
        assert_eq!(
            store.depth(),
            approximate,
            "{metric:?}: exact cost a round trip"
        );
    }
}

#[tokio::test]
async fn a_fold_rejects_a_second_metric_even_in_a_new_index() {
    // Two engines write the first rows of one index, same width, different metrics. The
    // fold creates the schema from the first lane's rows and rejects the other's -- before
    // M9d the reject pass skipped a new index and sealed both.
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), TenantId(32), LaneId(1));
    let b = Engine::new(Arc::clone(&store), TenantId(32), LaneId(2));
    a.write_as(
        "idx",
        vec![Document::new("a", vec![1.0, 2.0])],
        Metric::CosineDistance,
    )
    .await
    .unwrap();
    a.flush().await.unwrap();
    b.write("idx", vec![Document::new("b", vec![3.0, 4.0])])
        .await
        .unwrap();
    b.flush().await.unwrap();
    a.fold().await.unwrap();
    let stats = a.index_stats("idx").await.unwrap().unwrap();
    assert_eq!(stats.schema.unwrap().metric, Metric::CosineDistance);
    assert_eq!(stats.rejected_rows, 1);
    assert_eq!(stats.documents, 1);
    let rows = a.scan("idx", None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "a");
    assert!(
        rows[0].attrs.is_empty(),
        "the row's metric was sealed: {:?}",
        rows[0].attrs
    );
}

#[tokio::test]
async fn the_metric_survives_a_head_round_trip() {
    let mut h = pstore_engine::Head::default();
    for (name, metric) in [
        ("c", Metric::CosineDistance),
        ("d", Metric::DotProduct),
        ("e", Metric::EuclideanSquared),
    ] {
        h.schemas.insert(
            name.to_owned(),
            pstore_engine::IndexSchema {
                dims: 3,
                text_field: String::new(),
                metric,
            },
        );
    }
    let decoded = pstore_engine::Head::decode(&h.encode()).unwrap();
    assert_eq!(decoded, h);
    // A metric naming no schema is not a HEAD this code wrote.
    let mut bytes = pstore_engine::Head::default().encode();
    bytes.truncate(bytes.len() - 4); // the empty metric section's count
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(b'x');
    bytes.push(1);
    assert!(pstore_engine::Head::decode(&bytes).is_err());
}
