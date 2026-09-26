//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A query vector of the wrong size — found by M7c, through the API.
//!
//! ⚠️ **The exact path refuses it and the approximate path did not.**
//! `Segment::search` compares the query's length against the row's and returns
//! `DimensionMismatch`. The dense leg built its `VecIndex` with `query.len()` as the
//! dimension, so the index took its idea of the dimension **from the query itself**: a
//! 2-dimensional query against a 4-dimensional segment produced scored, ranked, plausible
//! results. Wrong answers with a `200`, which is the worst failure mode a search engine has.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, SegmentWriter};
use pstore_query::{Fusion, Prefetch, Target, query};
use std::sync::Arc;

#[tokio::test]
async fn a_query_vector_of_the_wrong_dimension_is_refused_by_the_dense_leg() {
    let store = Arc::new(MemoryStore::new());
    let key = Key::new("0000/seg/dim");
    let mut w = SegmentWriter::new(8);
    for i in 0..32 {
        w.push(Document::new(
            format!("d{i:03}"),
            vec![i as f32, 1.0, 0.5, -0.25],
        ));
    }
    store.put(&key, w.finish()).await.unwrap();

    let target = Target {
        centroids: pstore_index::vec_index::centroid_key(&key),
        deleted: None,
        shadowed: false,
        segment: key.clone(),
    };
    // Four dimensions in, two out. ⚠️ Asserted on BOTH paths, because only one of them was
    // wrong and a fix that moves the check into the exact path alone would look green.
    let err = query(
        &*store,
        std::slice::from_ref(&target),
        &[Prefetch::Dense {
            field: pstore_format::DEFAULT_FIELD.to_owned(),
            query: vec![1.0, 2.0],
            limit: 5,
            tune: pstore_index::vec_index::Query::default(),
        }],
        Fusion::Rrf { k: 60.0 },
        5,
    )
    .await
    .expect_err("a two-dimensional query was answered over a four-dimensional segment");
    assert!(
        format!("{err}").contains("dimension"),
        "the refusal must name the dimension: {err}"
    );

    // And the right dimension still answers, so the check refuses only what it should.
    let hits = query(
        &*store,
        std::slice::from_ref(&target),
        &[Prefetch::Dense {
            field: pstore_format::DEFAULT_FIELD.to_owned(),
            query: vec![1.0, 1.0, 0.5, -0.25],
            limit: 5,
            tune: pstore_index::vec_index::Query::default(),
        }],
        Fusion::Rrf { k: 60.0 },
        5,
    )
    .await
    .unwrap();
    assert!(!hits.is_empty());
}
