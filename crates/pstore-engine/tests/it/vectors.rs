//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M64: a hit's vector, in client space, whether its row is folded or still unfolded.

use pstore_blob::{Accounted, MemoryStore};
use pstore_engine::{Answer, Consistency, Engine, Metric};
use pstore_format::Document;
use pstore_query::{Fusion, Hit, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;

const T: TenantId = TenantId(6400);

fn v(i: usize) -> Vec<f32> {
    let x = i as f32;
    vec![
        1.0 + x * 0.3,
        x.sin() * 2.0,
        (x * 0.7).cos() + 3.0,
        0.5 + (i % 3) as f32,
    ]
}

fn docs() -> Vec<Document> {
    (0..10)
        .map(|i| Document::new(format!("d{i}"), v(i)))
        .collect()
}

fn leg() -> Vec<Prefetch> {
    // A client's query: the engine puts it through the metric's own transform.
    let q = vec![0.4, 0.2, 0.9, 1.0];
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: q,
        limit: 20,
        tune: pstore_index::vec_index::Query {
            exact: true,
            ..pstore_index::vec_index::Query::default()
        },
    }]
}

async fn by_id(
    e: &Engine<pstore_blob::TenantView<MemoryStore>>,
    index: &str,
) -> BTreeMap<String, Vec<f32>> {
    let a = e
        .query_split_vectors(
            index,
            &leg(),
            None,
            Fusion::default(),
            20,
            Consistency::Eventual,
            None,
        )
        .await
        .unwrap();
    e.resolve_rows_with_vectors(&a)
        .into_iter()
        .map(|((id, ..), vector)| (id, vector.unwrap_or_else(|| panic!("no vector for a hit"))))
        .collect()
}

fn near(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| (x - y).abs() <= 1e-6 * (1.0 + y.abs()))
}

#[tokio::test]
async fn client_space_is_the_same_for_unfolded_and_folded_rows() {
    for metric in [
        Metric::CosineDistance,
        Metric::EuclideanSquared,
        Metric::DotProduct,
    ] {
        let acct = Arc::new(Accounted::new(MemoryStore::new()));
        let store = Arc::new(acct.as_tenant(T));
        let e = Engine::new(Arc::clone(&store), T, LaneId(1));
        // One index folded, the same rows in another left unfolded in this process.
        e.write_as("folded", docs(), metric).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
        e.write_as("fresh", docs(), metric).await.unwrap();

        let folded = by_id(&e, "folded").await;
        let fresh = by_id(&e, "fresh").await;
        assert_eq!(folded.len(), 10, "{metric:?}");
        assert_eq!(fresh.len(), 10, "{metric:?}");
        for i in 0..10 {
            let id = format!("d{i}");
            let want: Vec<f32> = match metric {
                Metric::CosineDistance => {
                    let n = v(i).iter().map(|x| x * x).sum::<f32>().sqrt();
                    v(i).iter().map(|x| x / n).collect()
                }
                // The raw vector: the euclidean transform's extra component is gone.
                Metric::EuclideanSquared | Metric::DotProduct => v(i),
            };
            assert!(
                near(&folded[&id], &want),
                "{metric:?} folded {id}: {:?}",
                folded[&id]
            );
            assert!(
                near(&fresh[&id], &want),
                "{metric:?} unfolded {id}: {:?}",
                fresh[&id]
            );
        }
    }
}

#[tokio::test]
async fn vectors_are_empty_unless_asked_for() {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write_as("docs", docs(), Metric::DotProduct)
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let a = e
        .query_split_as(
            "docs",
            &leg(),
            None,
            Fusion::default(),
            20,
            Consistency::Eventual,
            None,
        )
        .await
        .unwrap();
    assert!(!a.hits.is_empty());
    assert!(
        a.vectors.is_empty(),
        "no vectors unless asked: {}",
        a.vectors.len()
    );
    assert!(
        e.resolve_rows_with_vectors(&a)
            .iter()
            .all(|(_, vector)| vector.is_none())
    );
}

#[test]
fn a_hit_whose_row_vanished_does_not_misalign_the_vectors() {
    // Three hits; the middle one's id is gone. Its vector goes with it, and the third hit keeps
    // its own, not the second's.
    let hit = |row| Hit {
        segment: 0,
        row,
        score: 1.0,
    };
    let e = Engine::new(
        Arc::new(Accounted::new(MemoryStore::new()).as_tenant(T)),
        T,
        LaneId(1),
    );
    let answer = Answer {
        hits: vec![hit(0), hit(1), hit(2)],
        unfolded: Default::default(),
        unfolded_at: 9,
        ids: vec![Some("a".into()), None, Some("c".into())],
        attributes: vec![BTreeMap::new(), BTreeMap::new(), BTreeMap::new()],
        dists: vec![None, None, None],
        segments: Vec::new(),
        named: true,
        epoch: pstore_types::Epoch(0),
        watermarks: BTreeMap::new(),
        staleness: std::time::Duration::ZERO,
        fts: pstore_format::text::FullText::default(),
        vectors: vec![Some(vec![1.0]), Some(vec![2.0]), Some(vec![3.0])],
    };
    let got = e.resolve_rows_with_vectors(&answer);
    let pairs: Vec<(String, Option<Vec<f32>>)> = got
        .into_iter()
        .map(|((id, ..), vector)| (id, vector))
        .collect();
    assert_eq!(
        pairs,
        [
            ("a".to_owned(), Some(vec![1.0])),
            ("c".to_owned(), Some(vec![3.0]))
        ]
    );
}
