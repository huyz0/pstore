//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M198: HEAD records, per segment, the dense fields a query searches by clusters there, and
//! the record agrees with what a query's plan reports.

use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::{DEFAULT_FIELD, Document, VectorField};
use pstore_query::explain::collect;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(198);

/// A document of one of 12 topics. `a` is carried by every document; `b` by all but every
/// seventh, so no segment of more than six rows clusters it.
fn doc(i: usize) -> Document {
    let topic = i % 12;
    let mut s = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut r = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 2000) as f32 / 1000.0 - 1.0
    };
    let base = |t: usize, d: usize| ((t * 7 + d * 3) % 11) as f32 - 5.0;
    let v: Vec<f32> = (0..4).map(|k| base(topic, k) + 0.3 * r()).collect();
    let mut d = Document::new(format!("d{i:05}"), v.clone());
    d.vectors.insert(
        "a".into(),
        VectorField::Dense(vec![v.iter().map(|x| -x).collect()]),
    );
    if !i.is_multiple_of(7) {
        d.vectors.insert("b".into(), VectorField::Dense(vec![v]));
    }
    d
}

fn engine(store: &Arc<MemoryStore>, lane: u64) -> Engine<MemoryStore> {
    // `vector` clusters above 1,000 rows here; a named field at the default params, above
    // 4,000 (two lists).
    Engine::new(Arc::clone(store), T, LaneId(lane)).with_index_params(
        pstore_index::cluster::Params {
            exact_scan_threshold: 1_000,
            ..pstore_index::cluster::Params::default()
        },
    )
}

fn leg(field: &str) -> Prefetch {
    Prefetch::Dense {
        field: field.to_owned(),
        query: vec![-2.0, 1.0, 4.0, 0.0],
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }
}

/// For each field, `(segments HEAD names it in, known segments it does not)`, and the same
/// from a default query's plan.
async fn agree(e: &Engine<MemoryStore>, index: &str) -> Vec<(usize, usize)> {
    let head = e.head_for_test().await;
    let refs = &head.indexes[index];
    assert!(refs.iter().all(|r| r.ann.is_some()), "{refs:?}");
    let mut out = Vec::new();
    for field in [DEFAULT_FIELD, "a", "b"] {
        let named = refs
            .iter()
            .filter(|r| r.ann.as_ref().unwrap().contains(field))
            .count();
        let recorded = (named, refs.len() - named);
        let (got, x) = collect(e.query(index, &[leg(field)], Fusion::default(), 10)).await;
        got.unwrap();
        let l = &x.legs[0];
        assert_eq!(
            (l.approximate_segments, l.exact_segments),
            recorded,
            "{field}: {x:?}"
        );
        out.push(recorded);
    }
    out
}

#[tokio::test]
async fn heads_sets_agree_with_what_a_query_searches_by_clusters() {
    // Criterion 1: two segments above both thresholds, one below, rows left unfolded.
    let store = Arc::new(MemoryStore::new());
    let e = engine(&store, 1);
    for range in [0..5_000, 5_000..10_000, 10_000..10_200] {
        e.write("idx", range.map(doc).collect()).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.write("idx", (10_200..10_230).map(doc).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    // `vector` and `a` in the two big segments; `b` in none.
    assert_eq!(agree(&e, "idx").await, vec![(2, 1), (2, 1), (0, 3)]);
    // The small segment's set is known and empty, not unknown.
    let head = e.head_for_test().await;
    assert_eq!(
        head.indexes["idx"][2].ann,
        Some(std::collections::BTreeSet::new())
    );

    // From the store, in a fresh process.
    let fresh = engine(&store, 2);
    assert_eq!(agree(&fresh, "idx").await, vec![(2, 1), (2, 1), (0, 3)]);

    // A branch carries the source's sets.
    e.branch("idx", "copy").await.unwrap();
    let head = e.head_for_test().await;
    let sets = |i: &str| -> Vec<_> { head.indexes[i].iter().map(|r| r.ann.clone()).collect() };
    assert_eq!(sets("copy"), sets("idx"));

    // A compaction records what its own seal built, and the query agrees again.
    e.compact("idx").await.unwrap();
    let after = agree(&e, "idx").await;
    let head = e.head_for_test().await;
    let n = head.indexes["idx"].len();
    assert!(n < 3, "nothing merged");
    assert_eq!(after[2], (0, n), "`b` is still lacking rows");
    assert!(after[0].0 >= 1 && after[1].0 >= 1, "{after:?}");
}
