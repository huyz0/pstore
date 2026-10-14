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

//! M64: a hit's `vector` read in the same fetch as its row.
//!
//! The segment's `Vectors` section is its first dense field. Without `vector` it holds
//! another field, and with several vectors a row it holds an offset table: reading it as
//! `vector`'s rows would hand back misaligned floats and no error.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::text::FullText;
use pstore_format::{DEFAULT_FIELD, Document, SegmentWriter, VectorField};
use pstore_query::{Fusion, Prefetch, Target, query_rows_split};
use std::collections::HashSet;

fn v(i: usize) -> Vec<f32> {
    let x = i as f32;
    vec![x.sin(), x.cos(), (x * 0.5).sin(), 1.0 + x * 0.1]
}

async fn put(store: &MemoryStore, name: &str, docs: Vec<Document>, shadowed: bool) -> Target {
    let mut w = SegmentWriter::new(8);
    for d in docs {
        w.push(d);
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
        shadowed,
        segment: key,
    }
}

fn dense(limit: usize) -> Prefetch {
    Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: vec![0.3, 0.9, -0.2, 1.0],
        limit,
        tune: pstore_index::vec_index::Query {
            exact: true,
            ..pstore_index::vec_index::Query::default()
        },
    }
}

async fn run(
    store: &MemoryStore,
    targets: &[Target],
    legs: &[Prefetch],
    shadow: &HashSet<String>,
    vectors: bool,
) -> Vec<(usize, usize, Document)> {
    query_rows_split(
        store,
        targets,
        legs,
        &FullText::default(),
        None,
        shadow,
        Fusion::default(),
        40,
        Vec::new(),
        vectors,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|(h, d, _)| (h.segment, h.row, d.unwrap()))
    .collect()
}

#[tokio::test]
async fn vector_rows_ride_with_the_rows() {
    let store = MemoryStore::new();
    let docs = |range: std::ops::Range<usize>| -> Vec<Document> {
        range
            .map(|i| Document::new(format!("d{i:02}"), v(i)))
            .collect()
    };
    let a = put(&store, "a", docs(0..20), false).await;
    let b = put(&store, "b", docs(20..35), false).await;
    let targets = [a, b];
    let none = HashSet::new();

    // With the flag, every document carries the vector its segment holds at its row.
    let got = run(&store, &targets, &[dense(40)], &none, true).await;
    assert_eq!(got.len(), 35);
    for (_, _, d) in &got {
        let i: usize = d.id[1..].parse().unwrap();
        assert_eq!(d.vector(), v(i).as_slice(), "{}", d.id);
    }
    // Without it, none.
    let got = run(&store, &targets, &[dense(40)], &none, false).await;
    assert_eq!(got.len(), 35);
    assert!(got.iter().all(|(_, _, d)| d.vector().is_empty()));

    // With a nonempty shadow, the candidates are fetched before fusion, and carry theirs.
    let shadowed = [
        Target {
            shadowed: true,
            ..targets[0].clone()
        },
        targets[1].clone(),
    ];
    let shadow: HashSet<String> = ["d03".to_owned(), "d25".to_owned()].into();
    let got = run(&store, &shadowed, &[dense(40)], &shadow, true).await;
    assert!(
        !got.iter().any(|(_, _, d)| d.id == "d03"),
        "shadowed in a shadowed segment"
    );
    assert!(
        got.iter().any(|(_, _, d)| d.id == "d25"),
        "shadowing only reaches shadowed segments"
    );
    assert_eq!(got.len(), 34);
    for (_, _, d) in &got {
        let i: usize = d.id[1..].parse().unwrap();
        assert_eq!(d.vector(), v(i).as_slice(), "{}", d.id);
    }
}

#[tokio::test]
async fn a_segment_whose_vectors_section_is_not_vector_resolves_without_one() {
    let store = MemoryStore::new();
    // Only a named dense field: it takes `Section::Vectors`, but it is not `vector`.
    let only_named: Vec<Document> = (0..10)
        .map(|i| {
            let mut d = Document::new(format!("n{i:02}"), Vec::new());
            d.vectors.remove(DEFAULT_FIELD);
            d.vectors.insert(
                "emb".to_owned(),
                VectorField::Dense(vec![vec![i as f32, 1.0, 2.0]]),
            );
            d
        })
        .collect();
    // `vector` holding several vectors a row: its section is an offset table.
    let several: Vec<Document> = (0..10)
        .map(|i| {
            let mut d = Document::new(format!("s{i:02}"), v(i));
            d.vectors.insert(
                DEFAULT_FIELD.to_owned(),
                VectorField::Dense(vec![v(i), v(i + 1), v(i + 2)]),
            );
            d
        })
        .collect();
    let named = put(&store, "named", only_named, false).await;
    let multi = put(&store, "several", several, false).await;
    let seg = pstore_format::Segment::open(&store, &named.segment)
        .await
        .unwrap();
    assert!(!seg.reads_vector_rows(), "a named field only");
    let seg = pstore_format::Segment::open(&store, &multi.segment)
        .await
        .unwrap();
    assert!(!seg.reads_vector_rows(), "several vectors a row");

    let leg = |field: &str, q: Vec<f32>| Prefetch::Multi {
        field: field.to_owned(),
        query: vec![q],
        limit: 20,
    };
    let got = run(
        &store,
        &[named],
        &[leg("emb", vec![1.0, 0.0, 0.0])],
        &HashSet::new(),
        true,
    )
    .await;
    assert_eq!(got.len(), 10);
    assert!(
        got.iter().all(|(_, _, d)| d.vector().is_empty()),
        "no vector read"
    );
    let got = run(
        &store,
        &[multi],
        &[leg(DEFAULT_FIELD, vec![1.0, 0.0, 0.0, 0.0])],
        &HashSet::new(),
        true,
    )
    .await;
    assert_eq!(got.len(), 10);
    assert!(
        got.iter().all(|(_, _, d)| d.vector().is_empty()),
        "no misaligned floats"
    );
}
