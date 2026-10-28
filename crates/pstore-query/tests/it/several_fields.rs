//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! One query over several text fields (M153): each leg analyzed and scored under its own
//! field's `FullText`, against its own field's statistics.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::text::{self, Analyzer, FullText, Language};
use pstore_format::{Document, Value};
use pstore_index::cluster::Params;
use pstore_index::vec_index;
use pstore_query::{
    Fusion, Hit, Prefetch, QueryError, Target, Texts, Weights, query_rows_filtered_texts,
    query_with, query_with_texts,
};
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;
use std::collections::{BTreeMap, HashSet};

const DIM: usize = 4;

fn fts_a() -> FullText {
    FullText::default()
}

/// `b`'s: English stemming, and `k1` and `b` other than the default.
fn fts_b() -> FullText {
    FullText {
        analyzer: Analyzer {
            language: Language::English,
            stemming: true,
            ..Analyzer::default()
        },
        k1: 1.6,
        b: 0.4,
    }
}

fn texts() -> Texts {
    Texts {
        default: fts_a(),
        fields: BTreeMap::from([("b".to_owned(), fts_b())]),
    }
}

const WORDS: [&str; 6] = ["quarterly", "growth", "margin", "fox", "revenue", "run"];

/// Rows `base..base + n`: `a` in most, and `b` -- some holding "run" -- only `with_b`, and
/// never in every row.
fn rows(base: usize, n: usize, with_b: bool) -> Vec<Document> {
    (base..base + n)
        .map(|i| {
            let mut d = Document::new(
                format!("d{i:05}"),
                (0..DIM).map(|j| ((i + j) % 7) as f32 / 7.0).collect(),
            );
            let w = |k: usize| WORDS[(i * k + k / 2) % WORDS.len()];
            if i % 4 != 0 {
                d.attrs.insert(
                    "a".into(),
                    Value::Str(format!("{} {} {}", w(1), w(2), w(5))),
                );
            }
            if with_b && i % 5 != 0 {
                d.attrs
                    .insert("b".into(), Value::Str(format!("{} {}", w(3), w(7))));
            }
            d
        })
        .collect()
}

async fn put<S: BlobStore>(store: &S, name: &str, docs: &[Document]) -> Target {
    let (a, b) = (fts_a().analyzer, fts_b().analyzer);
    let built = vec_index::try_build_all_fields(
        docs,
        Params {
            exact_scan_threshold: 10_000,
            ..Params::default()
        },
        pstore_format::DEFAULT_FIELD,
        None,
        &[("a", &a), ("b", &b)],
        &[],
    )
    .unwrap();
    let key = Key::new(format!("t/idx/{name}.seg"));
    store.put(&key, built.segment).await.unwrap();
    let has_text = built.text_dictionary.is_some();
    if let Some(d) = built.text_dictionary {
        store
            .put(&text::dict_key(&key), bytes::Bytes::from(d))
            .await
            .unwrap();
    }
    Target {
        segment_len: None,
        centroids: None,
        deleted: None,
        sparse_dict: false,
        text_dict: has_text,
        fde_clusters: false,
        shadowed: false,
        segment: key,
    }
}

fn leg(field: &str, q: &str) -> Prefetch {
    Prefetch::Text {
        field: field.to_owned(),
        query: q.to_owned(),
        limit: 500,
    }
}

fn sum(n: usize) -> Fusion {
    Fusion::Sum {
        weights: Weights::of(&vec![1.0; n]).unwrap(),
    }
}

fn scores(hits: &[Hit]) -> BTreeMap<(usize, usize), f32> {
    hits.iter().map(|h| ((h.segment, h.row), h.score)).collect()
}

/// Criterion 2: each leg is scored with the statistics a query over its field alone computes,
/// over segments that differ in the fields they carry.
///
/// `b` is carried by two segments of three, so its statistics are neither one segment's own
/// (a lookup that falls back to the segment's df would score differently) nor all three's
/// (summing a segment without `b` changes `doc_count`). Each oracle is a single-field query
/// over exactly the segments carrying the field, under that field's own `FullText` -- a path
/// on which `Texts::default` is the field's.
#[tokio::test]
async fn each_field_is_scored_by_its_own_statistics() {
    let s = MemoryStore::new();
    let targets = [
        put(&s, "ab", &rows(0, 40, true)).await,
        put(&s, "a", &rows(40, 40, false)).await,
        put(&s, "ab2", &rows(80, 30, true)).await,
    ];
    let legs = [leg("a", "quarterly growth"), leg("b", "Running")];
    let both = query_rows_filtered_texts(
        &s,
        &targets,
        &legs,
        &texts(),
        None,
        &HashSet::new(),
        sum(2),
        500,
    )
    .await
    .unwrap();
    let got: BTreeMap<(usize, usize), f32> = both
        .iter()
        .map(|(h, _, _)| ((h.segment, h.row), h.score))
        .collect();
    let a = scores(
        &query_with(&s, &targets, &legs[..1], &fts_a(), sum(1), 500)
            .await
            .unwrap(),
    );
    // `b`'s carriers are segments 0 and 2; the oracle's segment 1 is the query's 2.
    let carriers = [targets[0].clone(), targets[2].clone()];
    let b: BTreeMap<(usize, usize), f32> = scores(
        &query_with(&s, &carriers, &legs[1..], &fts_b(), sum(1), 500)
            .await
            .unwrap(),
    )
    .into_iter()
    .map(|((seg, row), v)| (([0, 2][seg], row), v))
    .collect();
    assert!(
        b.keys().any(|(seg, _)| *seg == 0) && b.keys().any(|(seg, _)| *seg == 2),
        "\"Running\" stemmed matched no \"run\" in one of b's segments"
    );
    let mut want: BTreeMap<(usize, usize), f32> = BTreeMap::new();
    for (k, v) in a.iter().chain(&b) {
        // In leg order, as `fuse` adds: `a`'s, then `b`'s.
        *want.entry(*k).or_insert(0.0) += *v;
    }
    assert_eq!(got.len(), want.len());
    for (k, w) in &want {
        assert_eq!(
            got[k].to_bits(),
            w.to_bits(),
            "{k:?}: {} against {w}",
            got[k]
        );
    }
}

/// Criterion 3: a field no segment carries is refused beside one that is; the old entry
/// points keep M151's refusal. (Its third point, a phased share of several fields refused,
/// M157 reverses.)
#[tokio::test]
async fn what_several_fields_cannot_mean_is_refused() {
    let counting = DepthCounting::new(MemoryStore::new());
    let targets = [
        put(&counting, "l", &rows(0, 40, true)).await,
        put(&counting, "r", &rows(40, 40, true)).await,
    ];
    let typo = [leg("a", "growth"), leg("d", "growth")];
    let e = query_rows_filtered_texts(
        &counting,
        &targets,
        &typo,
        &texts(),
        None,
        &HashSet::new(),
        sum(2),
        10,
    )
    .await
    .unwrap_err();
    // M155: naming the field no segment carries, not the one beside it.
    assert!(
        matches!(e, QueryError::UnknownTextField(ref f) if f == "d"),
        "{e:?}"
    );
    let two = [leg("a", "growth"), leg("b", "run")];
    let e = query_with(&counting, &targets, &two, &fts_a(), sum(2), 10)
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            QueryError::Unimplemented("text legs over different fields")
        ),
        "{e:?}"
    );
    // M157: a phased share beside several fields is taken, a `Stats` a field; its refusal,
    // this test's third point until then, is reversed.
}

/// Criterion 5: two fields cost one field's rounds, and their requests add but for one open;
/// each field's bytes are read once, and the sidecar once.
#[tokio::test]
async fn two_fields_cost_one_open() {
    let t = TenantId(15300);
    let acct = Accounted::new(MemoryStore::new());
    let counting = DepthCounting::new(acct.as_tenant(t));
    let target = put(&counting, "ab", &rows(0, 40, true)).await;
    let targets = std::slice::from_ref(&target);
    let dict = text::dict_key(&target.segment);
    let raw = counting.inner().get(&dict).await.unwrap();
    let (la, lb) = (leg("a", "quarterly growth"), leg("b", "Running"));
    let measure = |hits: Vec<Hit>| {
        let cost = (
            counting.depth(),
            counting.requests(),
            acct.bytes_in(&target.segment, 0..u64::MAX),
            acct.bytes_in(&dict, 0..raw.len() as u64),
        );
        (hits.len(), cost)
    };
    let mut run = Vec::new();
    for (legs, fts) in [
        (vec![la.clone()], fts_a()),
        (vec![lb.clone()], fts_b()),
        (vec![leg("a", "zzzz")], fts_a()),
    ] {
        counting.reset();
        acct.record_ranges();
        run.push(measure(
            query_with(&counting, targets, &legs, &fts, sum(1), 10)
                .await
                .unwrap(),
        ));
    }
    counting.reset();
    acct.record_ranges();
    let ab = measure(
        query_with_texts(&counting, targets, &[la, lb], &texts(), sum(2), 10)
            .await
            .unwrap(),
    );
    let [(na, a), (nb, b), (n0, open)] = [run[0], run[1], run[2]];
    assert!(na > 0 && nb > 0 && n0 == 0 && ab.0 > 0);
    assert_eq!(ab.1.0, a.0, "depth");
    assert_eq!(ab.1.1, a.1 + b.1 - open.1, "requests");
    assert_eq!(ab.1.2, a.2 + b.2 - open.2, "segment bytes");
    // The sidecar is a whole-object GET, which byte accounting does not record: that it is
    // fetched once is the request count's `- open`, which holds it.
    assert_eq!(ab.1.3, a.3, "the sidecar read by range");
}
