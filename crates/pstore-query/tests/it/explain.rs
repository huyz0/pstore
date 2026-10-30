//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M165: a query's plan, as it ran -- each figure equal to what the test computes from the
//! segments it built.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{DEFAULT_FIELD, Document, Value, text};
use pstore_index::cluster::Params;
use pstore_index::vec_index;
use pstore_query::explain::{Because, Explain, Leg, collect};
use pstore_query::{Fusion, Op, Predicate, Prefetch, Target, Weights, query, query_rows_filtered};
use std::collections::HashSet;

const DIM: usize = 8;

fn doc(i: usize) -> Document {
    let mut d = Document::new(
        format!("d{i:05}"),
        (0..DIM)
            .map(|j| ((i * 7 + j * 3) % 13) as f32 / 13.0)
            .collect(),
    );
    d.attrs.insert(
        text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str(format!("document quarterly revenue w{}", i % 29)),
    );
    d.attrs.insert("n".to_owned(), Value::Int(i as i64));
    d
}

/// A segment of `docs`, clustered above 200 rows into lists of about 40.
async fn put(store: &MemoryStore, name: &str, docs: &[Document]) -> (Target, usize) {
    let built = vec_index::build_all(
        docs,
        Params {
            target_list_size: 40,
            exact_scan_threshold: 200,
            // No boundary replicas: a segment's index rows are its rows.
            replicas: 0,
            ..Params::default()
        },
        DEFAULT_FIELD,
        None,
        Some(text::DEFAULT_TEXT_FIELD),
    );
    let seg = Key::new(format!("t/idx/{name}.seg"));
    let cen = vec_index::centroid_key(&seg);
    store.put(&seg, built.segment).await.unwrap();
    let lists = built.centroids.as_ref().map_or(0, |c| c.spans.len());
    if let Some(c) = &built.centroids {
        store
            .put(&cen, bytes::Bytes::from(c.encode()))
            .await
            .unwrap();
    }
    store
        .put(
            &text::dict_key(&seg),
            bytes::Bytes::from(built.text_dictionary.unwrap()),
        )
        .await
        .unwrap();
    let target = Target {
        segment_len: None,
        centroids: Some(cen),
        deleted: None,
        sparse_dict: false,
        text_dict: true,
        fde_clusters: false,
        shadowed: false,
        segment: seg,
    };
    (target, lists)
}

struct Fixture {
    store: MemoryStore,
    targets: Vec<Target>,
    /// Each segment's documents, and its lists (0 when scanned exactly).
    segments: Vec<(Vec<Document>, usize)>,
}

/// Two clustered segments of 400 and one of 50, below the threshold, scanned exactly.
async fn fixture() -> Fixture {
    let store = MemoryStore::new();
    let mut targets = Vec::new();
    let mut segments = Vec::new();
    for (name, range) in [("a", 0..400), ("b", 400..800), ("c", 800..850)] {
        let docs: Vec<Document> = range.map(doc).collect();
        let (t, lists) = put(&store, name, &docs).await;
        targets.push(t);
        segments.push((docs, lists));
    }
    Fixture {
        store,
        targets,
        segments,
    }
}

fn text_leg(q: &str, limit: usize) -> Prefetch {
    Prefetch::Text {
        field: text::DEFAULT_TEXT_FIELD.to_owned(),
        query: q.to_owned(),
        limit,
        last_as_prefix: false,
    }
}

fn dense_leg(limit: usize, exact: bool) -> Prefetch {
    Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: (0..DIM).map(|j| j as f32 / DIM as f32).collect(),
        limit,
        tune: vec_index::Query {
            exact,
            ..vec_index::Query::default()
        },
    }
}

/// A text leg's expected figures: `matches(segment)` documents match in each segment.
fn text_figures(limit: usize, matches: &[usize], requested: &[usize], kept: &[usize]) -> Leg {
    Leg {
        kind: "text",
        field: text::DEFAULT_TEXT_FIELD.to_owned(),
        limit,
        requested: requested.iter().sum(),
        candidates: matches
            .iter()
            .zip(requested)
            .map(|(m, r)| (*m).min(*r))
            .sum(),
        kept: kept.iter().sum(),
        ..Leg::default()
    }
}

/// The documents of each segment whose text holds `w{k}`.
fn tagged(f: &Fixture, k: usize) -> Vec<usize> {
    f.segments
        .iter()
        .map(|(docs, _)| docs.iter().filter(|d| doc_n(d) % 29 == k).count())
        .collect()
}

fn doc_n(d: &Document) -> usize {
    match d.attrs.get("n") {
        Some(Value::Int(n)) => *n as usize,
        _ => panic!("no n"),
    }
}

#[tokio::test]
async fn explain_records_each_segment_leg() {
    let f = fixture().await;
    let lists: Vec<usize> = f.segments.iter().map(|(_, l)| *l).collect();
    assert!(lists[0] >= 8 && lists[1] >= 8 && lists[2] == 0, "{lists:?}");
    let rows: Vec<usize> = f.segments.iter().map(|(d, _)| d.len()).collect();
    let w3 = tagged(&f, 3);

    // Criterion 1 and 4: limited, two text legs around a dense one, in request order.
    let legs = [
        text_leg("revenue", 5),
        dense_leg(10, false),
        text_leg("w3", 4),
    ];
    let (hits, e) = collect(query(
        &f.store,
        &f.targets,
        &legs,
        Fusion::Rrf { k: 60.0 },
        10,
    ))
    .await;
    // Without collecting, the same answer.
    let plain = query(&f.store, &f.targets, &legs, Fusion::Rrf { k: 60.0 }, 10)
        .await
        .unwrap();
    assert_eq!(hits.unwrap(), plain);
    let dense = Leg {
        kind: "dense",
        field: DEFAULT_FIELD.to_owned(),
        limit: 10,
        requested: 30,
        candidates: 30,
        kept: 30,
        approximate_segments: 2,
        exact_segments: 1,
        probes: lists[0].min(8) + lists[1].min(8),
        ..Leg::default()
    };
    assert_eq!(
        e,
        Explain {
            segments: 3,
            shadow: 0,
            remote_segments: 0,
            filter: false,
            fusion: "rrf",
            legs: vec![
                text_figures(5, &rows, &[5, 5, 5], &[5, 5, 5]),
                dense.clone(),
                text_figures(
                    4,
                    &w3,
                    &[4, 4, 4],
                    &w3.iter().map(|m| (*m).min(4)).collect::<Vec<_>>()
                ),
            ],
        }
    );

    // `exact`: every segment scanned, nothing probed.
    let (_, e) = collect(query(
        &f.store,
        &f.targets,
        &[dense_leg(10, true)],
        Fusion::Rrf { k: 60.0 },
        10,
    ))
    .await;
    assert_eq!(
        e.legs,
        vec![Leg {
            approximate_segments: 0,
            exact_segments: 3,
            probes: 0,
            ..dense.clone()
        }]
    );

    // Criterion 2: a filter makes every leg exhaustive; `kept` is what the mask admits, cut
    // to each segment's room.
    let below = Predicate::Cmp("n".to_owned(), Op::Lt, Value::Int(420));
    let admitted: Vec<usize> = f
        .segments
        .iter()
        .map(|(d, _)| d.iter().filter(|d| doc_n(d) < 420).count())
        .collect();
    let (_, e) = collect(query_rows_filtered(
        &f.store,
        &f.targets,
        &[dense_leg(10, false), text_leg("w3", 4)],
        &text::FullText::default(),
        Some(&below),
        &HashSet::new(),
        Fusion::Rrf { k: 60.0 },
        10,
    ))
    .await;
    let w3_below: Vec<usize> = f
        .segments
        .iter()
        .map(|(d, _)| {
            d.iter()
                .filter(|d| doc_n(d) < 420 && doc_n(d) % 29 == 3)
                .count()
        })
        .collect();
    assert!(e.filter);
    assert_eq!(
        e.legs,
        vec![
            Leg {
                exhaustive: Some(Because::Filter),
                requested: rows.iter().sum(),
                candidates: rows.iter().sum(),
                kept: admitted.iter().map(|a| (*a).min(10)).sum(),
                probes: lists[0] + lists[1],
                ..dense.clone()
            },
            Leg {
                exhaustive: Some(Because::Filter),
                ..text_figures(
                    4,
                    &w3,
                    &rows,
                    &w3_below.iter().map(|m| (*m).min(4)).collect::<Vec<_>>()
                )
            },
        ]
    );

    // Under `sum`, every leg is whole.
    let (_, e) = collect(query(
        &f.store,
        &f.targets,
        &[dense_leg(10, false), text_leg("w3", 4)],
        Fusion::Sum {
            weights: Weights::ONE,
        },
        10,
    ))
    .await;
    assert_eq!(e.fusion, "sum");
    assert!(
        e.legs.iter().all(
            |l| l.exhaustive == Some(Because::Sum) && l.requested == rows.iter().sum::<usize>()
        ),
        "{e:?}"
    );

    // Criterion 3: deleted rows widen every leg by their count, in their segment.
    let mut targets = f.targets.clone();
    let dv = Key::new("t/idx/a.del");
    f.store
        .put(
            &dv,
            bytes::Bytes::from(pstore_query::deletes::encode(&HashSet::from([0, 1, 2]))),
        )
        .await
        .unwrap();
    targets[0].deleted = Some(dv);
    let (_, e) = collect(query(
        &f.store,
        &targets,
        &[dense_leg(10, false)],
        Fusion::Rrf { k: 60.0 },
        10,
    ))
    .await;
    let leg = &e.legs[0];
    assert!(leg.widened && leg.exhaustive.is_none(), "{leg:?}");
    assert_eq!(leg.requested, 30 + 3);
    assert!(leg.kept <= 30 && leg.candidates >= leg.kept, "{leg:?}");
}

#[tokio::test]
async fn a_query_not_collected_records_nothing_and_a_collection_reports_its_last_run() {
    let f = fixture().await;
    let legs = [dense_leg(10, false)];
    // Recording outside a collection is a no-op, not a panic.
    query(&f.store, &f.targets, &legs, Fusion::Rrf { k: 60.0 }, 10)
        .await
        .unwrap();
    // Two runs in one collection: the last one's figures, never their sum.
    let (_, twice) = collect(async {
        query(&f.store, &f.targets, &legs, Fusion::Rrf { k: 60.0 }, 10)
            .await
            .unwrap();
        query(&f.store, &f.targets, &legs, Fusion::Rrf { k: 60.0 }, 10)
            .await
            .unwrap()
    })
    .await;
    let (_, once) = collect(query(
        &f.store,
        &f.targets,
        &legs,
        Fusion::Rrf { k: 60.0 },
        10,
    ))
    .await;
    assert_eq!(twice, once);
    assert_eq!(once.legs[0].requested, 30);
}

#[tokio::test]
async fn the_fresh_segment_is_in_no_figure() {
    let f = fixture().await;
    let (_, e) = collect(async {
        pstore_query::explain::fresh(Some(2));
        query(
            &f.store,
            &f.targets,
            &[dense_leg(10, false)],
            Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
    })
    .await;
    assert_eq!(e.segments, 2);
    let leg = &e.legs[0];
    assert_eq!(
        (leg.requested, leg.approximate_segments, leg.exact_segments),
        (20, 2, 0)
    );
}

/// A fresh ordinal past this run's targets names none of them: every target is a segment.
#[tokio::test]
async fn a_fresh_ordinal_past_the_targets_names_none() {
    let f = fixture().await;
    let n = f.targets.len();
    let (_, e) = collect(async {
        pstore_query::explain::fresh(Some(n));
        query(
            &f.store,
            &f.targets,
            &[dense_leg(10, false)],
            Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
    })
    .await;
    assert_eq!(e.segments, n);
}
