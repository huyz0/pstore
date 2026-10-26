//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M77: a dense leg over a named field scans that field's own vectors.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, SegmentWriter, VectorField};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch, Target, Weights, query};

fn tag(r: usize) -> Vec<f32> {
    let x = (r * 13) as f32;
    vec![x.sin(), (x * 0.7).cos(), (x * 0.3).sin()]
}

fn doc(r: usize, with_tag: bool, wide: bool) -> Document {
    let x = r as f32;
    // `vector`'s vectors point nowhere near `tag`'s, so a leg that searched the wrong field
    // would rank differently.
    let mut d = Document::new(format!("d{r}"), vec![x.cos(), x.sin(), 1.0, 0.5]);
    if with_tag {
        d.vectors
            .insert("tag".into(), VectorField::Dense(vec![tag(r)]));
    }
    if wide {
        d.vectors
            .insert("late".into(), VectorField::Dense(vec![tag(r), tag(r + 1)]));
    }
    d
}

async fn segment(
    store: &MemoryStore,
    name: &str,
    rows: std::ops::Range<usize>,
    tagged: bool,
) -> Target {
    let mut w = SegmentWriter::new(8);
    for r in rows {
        w.push(doc(r, tagged, tagged));
    }
    let key = Key::new(format!("t/idx/{name}.seg"));
    store.put(&key, w.try_finish().unwrap()).await.unwrap();
    Target {
        segment_len: None,
        centroids: None,
        deleted: None,
        sparse_dict: false,
        text_dict: false,
        fde_clusters: false,
        shadowed: false,
        segment: key,
    }
}

fn raw() -> Fusion {
    Fusion::Max {
        weights: Weights::ONE,
    }
}

fn dense(field: &str, q: &[f32], limit: usize) -> Prefetch {
    Prefetch::Dense {
        field: field.to_owned(),
        query: q.to_vec(),
        limit,
        tune: Query::default(),
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[tokio::test]
async fn a_dense_leg_over_a_named_field_scans_that_field() {
    let store = MemoryStore::new();
    let targets = vec![
        segment(&store, "a", 0..30, true).await,
        segment(&store, "b", 30..50, true).await,
    ];
    let q = [0.3f32, -0.8, 0.5];
    let hits = query(&store, &targets, &[dense("tag", &q, 10)], raw(), 10)
        .await
        .unwrap();
    // The model: every row's `tag` scored, ranked by score then (segment, row).
    let mut want: Vec<(usize, usize, f32)> = Vec::new();
    for (s, rows) in [(0usize, 0..30usize), (1, 30..50)] {
        for (row, r) in rows.enumerate() {
            want.push((s, row, dot(&q, &tag(r))));
        }
    }
    want.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    want.truncate(10);
    let got: Vec<(usize, usize, f32)> = hits.iter().map(|h| (h.segment, h.row, h.score)).collect();
    assert_eq!(got, want);

    // It is not `vector`'s ranking: a 4-wide query over `vector` ranks other rows.
    let by_vector = query(
        &store,
        &targets,
        &[dense("vector", &[0.3, -0.8, 0.5, 0.1], 10)],
        raw(),
        10,
    )
    .await
    .unwrap();
    let a: Vec<(usize, usize)> = hits.iter().map(|h| (h.segment, h.row)).collect();
    let b: Vec<(usize, usize)> = by_vector.iter().map(|h| (h.segment, h.row)).collect();
    assert_ne!(a, b);
}

#[tokio::test]
async fn a_missing_or_unsuitable_field_is_answered_or_refused() {
    let store = MemoryStore::new();
    let tagged = segment(&store, "a", 0..20, true).await;
    let bare = segment(&store, "b", 20..40, false).await;
    let q = [0.3f32, -0.8, 0.5];
    // A segment without the field answers nothing; the other's rows are all there is.
    let hits = query(
        &store,
        &[tagged.clone(), bare],
        &[dense("tag", &q, 100)],
        raw(),
        100,
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 20);
    assert!(hits.iter().all(|h| h.segment == 0));
    // A field no segment carries is a typo, not an empty answer.
    let err = query(
        &store,
        std::slice::from_ref(&tagged),
        &[dense("nope", &q, 5)],
        raw(),
        5,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("no such"), "{err}");
    // A width that is not the field's names both widths.
    let err = query(
        &store,
        std::slice::from_ref(&tagged),
        &[dense("tag", &[1.0, 2.0], 5)],
        raw(),
        5,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains('3') && err.contains('2'), "{err}");
    // A field of several vectors a document is `multi`'s.
    let err = query(&store, &[tagged], &[dense("late", &q, 5)], raw(), 5)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("multi"), "{err}");
}
