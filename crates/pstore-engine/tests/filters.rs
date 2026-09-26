//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Filters on a clustered segment — M9b. The case a post-filter gets wrong: the admitted
//! documents sit in posting lists the unfiltered probe never reads.

use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::{Document, Value};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Predicate, Prefetch};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

const DIM: usize = 8;
const ROWS: usize = 600;

fn corpus() -> Vec<Document> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    (0..ROWS)
        .map(|i| {
            let mut d = Document::new(format!("d{i:04}"), (0..DIM).map(|_| next()).collect());
            d.attrs.insert("n".to_owned(), Value::Int(i as i64));
            d
        })
        .collect()
}

async fn folded(
    store: Arc<DepthCounting<MemoryStore>>,
) -> (Engine<DepthCounting<MemoryStore>>, Vec<Document>) {
    let e = Engine::new(store, TenantId(91), LaneId(0)).with_index_params(
        pstore_index::cluster::Params {
            target_list_size: 40,
            exact_scan_threshold: 100,
            // Replicated boundary rows, so index rows and data rows differ and a mask keyed on
            // the wrong one is wrong.
            replicas: 1,
            boundary: 0.05,
            ..pstore_index::cluster::Params::default()
        },
    );
    let docs = corpus();
    e.write("idx", docs.clone()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    (e, docs)
}

fn dense(query: &[f32], limit: usize) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: query.to_vec(),
        limit,
        tune: Query::default(),
    }]
}

async fn ids(
    e: &Engine<DepthCounting<MemoryStore>>,
    q: &[f32],
    filter: Option<&Predicate>,
    k: usize,
) -> BTreeSet<String> {
    let answer = e
        .query_filtered("idx", &dense(q, k), filter, Fusion::Rrf { k: 60.0 }, k)
        .await
        .unwrap();
    e.resolve(&answer).into_iter().map(|(id, _)| id).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[tokio::test]
async fn a_selective_filter_finds_matches_the_unfiltered_probe_never_reads() {
    let (e, docs) = folded(Arc::new(DepthCounting::new(MemoryStore::new()))).await;
    let q = docs[0].vector().to_vec();
    // The five documents FARTHEST from the query.
    let mut by_score: Vec<(f32, &str)> = docs
        .iter()
        .map(|d| (dot(d.vector(), &q), d.id.as_str()))
        .collect();
    by_score.sort_by(|a, b| a.0.total_cmp(&b.0));
    let far: BTreeSet<String> = by_score[..5]
        .iter()
        .map(|(_, id)| (*id).to_owned())
        .collect();

    // Premise: even asking for EVERY row, the unfiltered probe does not reach all of them --
    // they sit in lists `p = 8` never reads, so no post-filter could return them.
    let everything = ids(&e, &q, None, ROWS).await;
    assert!(
        everything.len() < ROWS,
        "the probe read every list; the fixture proves nothing"
    );
    assert!(
        !far.is_subset(&everything),
        "every far document was reachable unfiltered"
    );

    let only_far = Predicate::In(
        "id".to_owned(),
        far.iter().map(|id| Value::Str(id.clone())).collect(),
    );
    assert_eq!(ids(&e, &q, Some(&only_far), 5).await, far);
}

#[tokio::test]
async fn a_filter_adds_no_round_trip() {
    let store = Arc::new(DepthCounting::new(MemoryStore::new()));
    let (e, docs) = folded(Arc::clone(&store)).await;
    let q = docs[3].vector().to_vec();
    store.reset();
    ids(&e, &q, None, 10).await;
    let unfiltered = store.depth();
    store.reset();
    let even = Predicate::Cmp("n".to_owned(), pstore_query::Op::Lt, Value::Int(300));
    let got = ids(&e, &q, Some(&even), 10).await;
    assert_eq!(got.len(), 10);
    assert_eq!(
        store.depth(),
        unfiltered,
        "a filter cost a sequential round trip"
    );
}
