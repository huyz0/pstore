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

//! M61: a document's many vectors, searched by MaxSim (D-28).
//!
//! The scores are read through `max` fusion with weight 1, which passes one leg's raw score
//! through untouched -- so the leg's MaxSim is compared, bit for bit, with the test's own.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Segment, SegmentWriter, VectorField};
use pstore_query::{Fusion, Prefetch, Target, Weights, query};

const FIELD: &str = "late";

/// `MaxSim(q, d) = Σᵢ maxⱼ ⟨qᵢ, dⱼ⟩`, the model the leg must equal, summed in the same order.
fn maxsim(q: &[Vec<f32>], d: &[Vec<f32>]) -> Option<f32> {
    if d.is_empty() {
        return None;
    }
    Some(
        q.iter()
            .map(|qi| {
                d.iter()
                    .map(|dj| qi.iter().zip(dj).map(|(a, b)| a * b).sum::<f32>())
                    .fold(f32::NEG_INFINITY, f32::max)
            })
            .sum(),
    )
}

/// Row `r`'s vectors in `late`: between none and four, 3 dimensions each.
fn late(r: usize) -> Vec<Vec<f32>> {
    (0..r % 5)
        .map(|j| {
            let x = (r * 7 + j * 3) as f32;
            vec![x.sin(), (x * 0.7).cos(), (x * 0.3).sin()]
        })
        .collect()
}

fn doc(r: usize) -> Document {
    let x = r as f32;
    let mut d = Document::new(format!("d{r}"), vec![x.cos(), x.sin(), 1.0, 0.5]);
    let vs = late(r);
    if !vs.is_empty() {
        d.vectors.insert(FIELD.to_owned(), VectorField::Dense(vs));
    }
    d
}

async fn segment(store: &MemoryStore, name: &str, rows: std::ops::Range<usize>) -> Target {
    let mut w = SegmentWriter::new(8);
    for r in rows {
        w.push(doc(r));
    }
    let key = Key::new(format!("t/idx/{name}.seg"));
    store.put(&key, w.try_finish().unwrap()).await.unwrap();
    Target {
        segment_len: None,
        centroids: None,
        deleted: None,
        sparse_dict: false,
        text_dict: false,
        shadowed: false,
        segment: key,
    }
}

fn raw() -> Fusion {
    Fusion::Max {
        weights: Weights::ONE,
    }
}

fn multi(q: &[Vec<f32>], limit: usize) -> Prefetch {
    Prefetch::Multi {
        field: FIELD.to_owned(),
        query: q.to_vec(),
        limit,
    }
}

#[tokio::test]
async fn multi_scores_by_maxsim() {
    let store = MemoryStore::new();
    let targets = vec![
        segment(&store, "a", 0..23).await,
        segment(&store, "b", 23..40).await,
    ];
    let q = vec![
        vec![0.2, -0.5, 0.9],
        vec![-0.7, 0.1, 0.3],
        vec![0.4, 0.4, -0.1],
    ];
    let hits = query(&store, &targets, &[multi(&q, 100)], raw(), 100)
        .await
        .unwrap();
    // The model: every row holding vectors, scored, ranked by score then (segment, row).
    let mut want: Vec<(usize, usize, f32)> = Vec::new();
    for (s, rows) in [(0usize, 0..23usize), (1, 23..40)] {
        for (row, r) in rows.enumerate() {
            if let Some(score) = maxsim(&q, &late(r)) {
                want.push((s, row, score));
            }
        }
    }
    want.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then_with(|| (a.0, a.1).cmp(&(b.0, b.1)))
    });
    let got: Vec<(usize, usize, u32)> = hits
        .iter()
        .map(|h| (h.segment, h.row, h.score.to_bits()))
        .collect();
    let want: Vec<(usize, usize, u32)> =
        want.iter().map(|(s, r, x)| (*s, *r, x.to_bits())).collect();
    assert_eq!(got, want, "MaxSim, every row with vectors, none without");
    // The leg's limit cuts it.
    let top = query(&store, &targets, &[multi(&q, 5)], raw(), 5)
        .await
        .unwrap();
    assert_eq!(top.len(), 5);
    assert_eq!(
        top.iter().map(|h| (h.segment, h.row)).collect::<Vec<_>>(),
        want.iter()
            .take(5)
            .map(|(s, r, _)| (*s, *r))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_default_field_is_field_zero() {
    // `late` sorts before `vector`, and must still not take the legacy fixed-width section.
    let store = MemoryStore::new();
    let target = segment(&store, "mixed", 0..30).await;
    let seg = Segment::open(&store, &target.segment).await.unwrap();
    let first = seg.fields().first().map(|f| f.name.clone());
    assert_eq!(first.as_deref(), Some(pstore_format::DEFAULT_FIELD));
    // A dense leg over `vector` answers as over a segment without `late`.
    let plain = {
        let mut w = SegmentWriter::new(8);
        for r in 0..30 {
            let mut d = doc(r);
            d.vectors.remove(FIELD);
            w.push(d);
        }
        let key = Key::new("t/idx/plain.seg".to_owned());
        store.put(&key, w.try_finish().unwrap()).await.unwrap();
        Target {
            segment: key,
            ..target.clone()
        }
    };
    let dense = Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![0.3, 0.9, -0.2, 1.0],
        limit: 10,
        tune: pstore_index::vec_index::Query {
            exact: true,
            ..pstore_index::vec_index::Query::default()
        },
    };
    let with = query(
        &store,
        std::slice::from_ref(&target),
        std::slice::from_ref(&dense),
        raw(),
        10,
    )
    .await
    .unwrap();
    let without = query(&store, std::slice::from_ref(&plain), &[dense], raw(), 10)
        .await
        .unwrap();
    assert_eq!(format!("{with:?}"), format!("{without:?}"));
    // And `late` is searched in the same segment, by its own vectors.
    let q = vec![vec![0.1, 0.2, 0.3]];
    let hits = query(&store, &[target], &[multi(&q, 3)], raw(), 3)
        .await
        .unwrap();
    assert_eq!(hits.len(), 3);
    let best = (0..30)
        .filter_map(|r| maxsim(&q, &late(r)).map(|s| (r, s)))
        .fold(None::<(usize, f32)>, |b, (r, s)| match b {
            Some((_, bs)) if bs >= s => b,
            _ => Some((r, s)),
        })
        .unwrap();
    assert_eq!(
        (hits[0].row, hits[0].score.to_bits()),
        (best.0, best.1.to_bits())
    );
}

#[tokio::test]
async fn multi_errors_are_loud() {
    use pstore_format::FormatError;
    use pstore_query::QueryError;
    let store = MemoryStore::new();
    let targets = vec![
        segment(&store, "a", 0..23).await,
        segment(&store, "b", 23..40).await,
    ];
    // A field no searched segment carries: an error, never an empty answer.
    let unknown = Prefetch::Multi {
        field: "nowhere".to_owned(),
        query: vec![vec![0.1, 0.2, 0.3]],
        limit: 5,
    };
    let err = query(&store, &targets, &[unknown], raw(), 5)
        .await
        .unwrap_err();
    assert!(
        matches!(err, QueryError::Format(FormatError::UnknownField)),
        "{err:?}"
    );
    // A query vector of the wrong width, even after a vector of the right one.
    let short = multi(&[vec![0.1, 0.2, 0.3], vec![0.1, 0.2]], 5);
    let err = query(&store, &targets, &[short], raw(), 5)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            QueryError::Format(FormatError::DimensionMismatch {
                expected: 3,
                got: 2
            })
        ),
        "{err:?}"
    );
    // A dense leg over a field of several vectors a row is refused, never scored as one.
    let dense = Prefetch::Dense {
        field: FIELD.to_owned(),
        query: vec![0.1, 0.2, 0.3],
        limit: 5,
        tune: pstore_index::vec_index::Query::default(),
    };
    let err = query(&store, &targets, &[dense], raw(), 5)
        .await
        .unwrap_err();
    assert!(matches!(err, QueryError::Unimplemented(_)), "{err:?}");
}

#[tokio::test]
async fn a_multi_leg_is_widened_past_deleted_rows() {
    // A leg cut at its limit before deletes are dropped would answer short: the best rows
    // deleted, `limit` live ones are still owed.
    let store = MemoryStore::new();
    let mut target = segment(&store, "d", 0..40).await;
    let q = vec![vec![0.2, -0.5, 0.9], vec![-0.7, 0.1, 0.3]];
    let all = query(
        &store,
        std::slice::from_ref(&target),
        &[multi(&q, 100)],
        raw(),
        100,
    )
    .await
    .unwrap();
    let best: std::collections::HashSet<usize> = all.iter().take(4).map(|h| h.row).collect();
    let key = Key::new("t/idx/d.dv".to_owned());
    store
        .put(&key, pstore_query::deletes::encode(&best).into())
        .await
        .unwrap();
    target.deleted = Some(key);
    let got = query(&store, &[target], &[multi(&q, 3)], raw(), 3)
        .await
        .unwrap();
    assert_eq!(
        got.iter().map(|h| h.row).collect::<Vec<_>>(),
        all.iter()
            .skip(4)
            .take(3)
            .map(|h| h.row)
            .collect::<Vec<_>>()
    );
}
