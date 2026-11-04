//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M162: an index measures its own recall, at one snapshot, from queries sampled by a seed.

use pstore_blob::{Accounted, MemoryStore, OpClass, TenantView};
use pstore_engine::{Engine, EngineError, RECALL_ROW_BUDGET, Recall, RecallRequest, recall_probe};
use pstore_format::{Document, Value};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Op, Predicate, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::collections::HashSet;
use std::sync::Arc;

const T: TenantId = TenantId(162);
const DIM: usize = 16;

type E = Engine<TenantView<MemoryStore>>;

fn engine(acct: &Accounted<MemoryStore>, lane: u64) -> E {
    Engine::new(Arc::new(acct.as_tenant(T)), T, LaneId(lane))
}

/// Spread vectors, so a probe of 8 lists among many misses neighbours.
fn doc(i: usize) -> Document {
    let mut st = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let v = (0..DIM)
        .map(|_| {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            (st >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        })
        .collect();
    let mut d = Document::new(format!("d{i:05}"), v);
    d.attrs
        .insert("even".to_owned(), Value::Bool(i.is_multiple_of(2)));
    d
}

/// `n` documents folded into one clustered segment: lists of about 10 rows, so 8 probed
/// lists of hundreds miss most neighbours.
async fn clustered(acct: &Accounted<MemoryStore>, n: usize) -> E {
    let e = engine(acct, 1).with_index_params(pstore_index::cluster::Params {
        target_list_size: 10,
        exact_scan_threshold: 100,
        ..pstore_index::cluster::Params::default()
    });
    e.write("idx", (0..n).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e
}

fn req(num: usize, seed: u64) -> RecallRequest {
    RecallRequest {
        num,
        top_k: 10,
        filter: None,
        seed,
        ground_truth: true,
    }
}

fn dense(q: &[f32], top_k: usize, exact: bool) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: q.to_vec(),
        limit: top_k,
        tune: Query {
            exact,
            ..Query::default()
        },
    }]
}

/// The averages, computed by running both searches for each sampled query here.
async fn by_hand(
    e: &E,
    r: &Recall,
    top_k: usize,
    filter: Option<&Predicate>,
) -> (Option<f64>, f64, f64) {
    let (mut shares, mut ann, mut exh) = (Vec::new(), 0usize, 0usize);
    for g in &r.ground_truth {
        let a = e
            .query_filtered(
                "idx",
                &dense(&g.query_vector, top_k, false),
                filter,
                Fusion::default(),
                top_k,
            )
            .await
            .unwrap();
        let x = e
            .query_filtered(
                "idx",
                &dense(&g.query_vector, top_k, true),
                filter,
                Fusion::default(),
                top_k,
            )
            .await
            .unwrap();
        let a: HashSet<String> = a.ids.into_iter().flatten().collect();
        let x: Vec<String> = x.ids.into_iter().flatten().collect();
        let mine: Vec<String> = g
            .nearest_neighbors
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(mine, x, "the ground truth is the exhaustive search");
        ann += a.len();
        exh += x.len();
        if !x.is_empty() {
            shares.push(x.iter().filter(|id| a.contains(*id)).count() as f64 / x.len() as f64);
        }
    }
    let n = r.ground_truth.len() as f64;
    let avg = (!shares.is_empty()).then(|| shares.iter().sum::<f64>() / shares.len() as f64);
    (avg, ann as f64 / n, exh as f64 / n)
}

#[tokio::test]
async fn recall_matches_its_own_queries() {
    let acct = Accounted::new(MemoryStore::new());
    let e = clustered(&acct, 2000).await;
    let r = e.recall("idx", &req(8, 1)).await.unwrap().unwrap();
    assert_eq!((r.num, r.ground_truth.len()), (8, 8));
    let (avg, ann, exh) = by_hand(&e, &r, 10, None).await;
    assert_eq!(r.avg_recall, avg);
    assert_eq!((r.avg_ann_count, r.avg_exhaustive_count), (ann, exh));
    assert!(
        r.avg_recall.unwrap() < 1.0,
        "8 lists of 200 found every neighbour: {r:?}"
    );
    assert!(
        (r.avg_exhaustive_count - 10.0).abs() < f64::EPSILON,
        "{r:?}"
    );
    // The queries are what an approximate search for the seed's probe returns, in its order.
    let near = e
        .query_split_vectors(
            "idx",
            &dense(&recall_probe(1, DIM), 8, false),
            None,
            Fusion::default(),
            8,
            pstore_engine::Consistency::default(),
            None,
        )
        .await
        .unwrap();
    let sampled: Vec<Vec<f32>> = near.vectors.into_iter().flatten().collect();
    let got: Vec<Vec<f32>> = r
        .ground_truth
        .iter()
        .map(|g| g.query_vector.clone())
        .collect();
    assert_eq!(got, sampled);
    // Small enough to be scanned exactly: the approximate search is the exact one.
    let small = Accounted::new(MemoryStore::new());
    let e = engine(&small, 1);
    e.write("idx", (0..50).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let r = e.recall("idx", &req(5, 1)).await.unwrap().unwrap();
    assert_eq!((r.num, r.avg_recall), (5, Some(1.0)));
    // An index HEAD does not hold.
    assert_eq!(e.recall("absent", &req(5, 1)).await.unwrap(), None);
}

#[tokio::test]
async fn recall_is_reproducible() {
    let acct = Accounted::new(MemoryStore::new());
    let e = clustered(&acct, 500).await;
    let a = e.recall("idx", &req(5, 7)).await.unwrap().unwrap();
    let b = e.recall("idx", &req(5, 7)).await.unwrap().unwrap();
    assert_eq!(a, b);
    let c = e.recall("idx", &req(5, 8)).await.unwrap().unwrap();
    assert_ne!(
        a.ground_truth, c.ground_truth,
        "another seed samples other queries"
    );
    // Every sampled query is a document's vector.
    let written: Vec<Vec<f32>> = (0..500).map(|i| doc(i).vector().to_vec()).collect();
    for g in a.ground_truth.iter().chain(&c.ground_truth) {
        assert!(written.contains(&g.query_vector), "{:?}", g.query_vector);
    }
    // Fewer documents than asked: fewer queries.
    let few = Accounted::new(MemoryStore::new());
    let e = engine(&few, 1);
    e.write("idx", (0..3).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let r = e.recall("idx", &req(10, 0)).await.unwrap().unwrap();
    assert_eq!((r.num, r.ground_truth.len()), (3, 3));
}

#[tokio::test]
async fn recall_with_filters() {
    let acct = Accounted::new(MemoryStore::new());
    let e = clustered(&acct, 600).await;
    let even = Predicate::Cmp("even".to_owned(), Op::Eq, Value::Bool(true));
    let mut q = req(6, 3);
    q.filter = Some(even.clone());
    let r = e.recall("idx", &q).await.unwrap().unwrap();
    let (avg, ann, exh) = by_hand(&e, &r, 10, Some(&even)).await;
    assert_eq!(r.avg_recall, avg);
    assert_eq!((r.avg_ann_count, r.avg_exhaustive_count), (ann, exh));
    for g in &r.ground_truth {
        for (id, _) in &g.nearest_neighbors {
            let i: usize = id[1..].parse().unwrap();
            assert_eq!(i % 2, 0, "{id} is not even");
        }
    }
    // A filter nothing satisfies.
    q.filter = Some(Predicate::Cmp("even".to_owned(), Op::Eq, Value::Int(3)));
    let r = e.recall("idx", &q).await.unwrap().unwrap();
    assert_eq!(
        (r.avg_recall, r.avg_ann_count, r.avg_exhaustive_count),
        (None, 0.0, 0.0)
    );
    assert_eq!(r.num, 6);
}

#[tokio::test]
async fn recall_is_one_snapshot() {
    let acct = Accounted::new(MemoryStore::new());
    let e = clustered(&acct, 300).await;
    let before = e.recall("idx", &req(5, 2)).await.unwrap().unwrap();
    // Written and not folded: not measured.
    e.write("idx", (300..400).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    let unfolded = e.recall("idx", &req(5, 2)).await.unwrap().unwrap();
    assert_eq!(unfolded, before);
    // Folded: another snapshot.
    let folded = e.fold().await.unwrap();
    let after = e.recall("idx", &req(5, 2)).await.unwrap().unwrap();
    assert_eq!(after.epoch, folded);
    assert!(after.epoch > before.epoch);
    // One HEAD read whatever `num` is, no LIST and no write.
    let reader = engine(&acct, 9);
    let (r0, w0, l0) = (
        acct.count(T, OpClass::Read),
        acct.count(T, OpClass::Write),
        acct.count(T, OpClass::List),
    );
    let heads = |a: &Accounted<MemoryStore>| {
        a.ranges()
            .iter()
            .filter(|(k, _)| k.as_str().ends_with("HEAD"))
            .count()
    };
    acct.record_ranges();
    reader.recall("idx", &req(5, 2)).await.unwrap().unwrap();
    assert!(acct.count(T, OpClass::Read) > r0);
    assert_eq!(acct.count(T, OpClass::Write), w0);
    assert_eq!(acct.count(T, OpClass::List), l0);
    assert_eq!(heads(&acct), 1, "{:?}", acct.ranges());
}

#[tokio::test]
async fn recall_refusals() {
    let acct = Accounted::new(MemoryStore::new());
    let e = clustered(&acct, 300).await;
    // 300 rows a search: the budget allows num * 300, and twice that with a filter.
    let per = RECALL_ROW_BUDGET / 300;
    let fits = usize::try_from(per).unwrap();
    let mut q = req(fits, 0);
    q.ground_truth = false;
    q.top_k = 1;
    let reads = acct.count(T, OpClass::Read);
    q.num = fits + 1;
    assert!(matches!(
        e.recall("idx", &q).await,
        Err(EngineError::RecallTooLarge { .. })
    ));
    q.num = fits / 2 + 1;
    q.filter = Some(Predicate::Cmp("even".to_owned(), Op::Eq, Value::Bool(true)));
    assert!(matches!(
        e.recall("idx", &q).await,
        Err(EngineError::RecallTooLarge { .. })
    ));
    assert_eq!(acct.count(T, OpClass::Read) - reads, 2, "only HEAD, twice");
    // An index HEAD holds with no default vector dimensions: nothing to measure, and
    // refused from HEAD alone.
    e.commit_head_for_test(|h| {
        h.indexes.insert("bare".to_owned(), Vec::new());
    })
    .await
    .unwrap();
    let reads = acct.count(T, OpClass::Read);
    assert!(matches!(
        e.recall("bare", &req(5, 0)).await,
        Err(EngineError::RecallNeedsVectors(_))
    ));
    assert_eq!(acct.count(T, OpClass::Read) - reads, 1, "only HEAD");
}
