//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `last_as_prefix` on a BM25 text leg (M180): the last raw token a literal prefix, worth 1.0
//! once to a row with a dictionary term starting with it, and the rest scored by BM25.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::text::{self, Analyzer, FullText, Language, TOKEN_SEPARATOR, Tokenizer};
use pstore_format::{Document, Value};
use pstore_index::cluster::Params;
use pstore_index::vec_index;
use pstore_query::{Fusion, Hit, Prefetch, Target, Weights, query_with};
use pstore_testkit::depth::DepthCounting;
use std::collections::{BTreeMap, BTreeSet};

const DIM: usize = 4;
const FIELD: &str = text::DEFAULT_TEXT_FIELD;

fn english() -> Analyzer {
    Analyzer {
        language: Language::English,
        stemming: true,
        remove_stopwords: true,
        ..Analyzer::default()
    }
}

fn case_sensitive() -> Analyzer {
    Analyzer {
        case_sensitive: true,
        ..Analyzer::default()
    }
}

fn folding() -> Analyzer {
    Analyzer {
        ascii_folding: true,
        ..Analyzer::default()
    }
}

fn pre_tokenized() -> Analyzer {
    Analyzer {
        tokenizer: Tokenizer::PreTokenizedArray,
        ..Analyzer::default()
    }
}

fn fts(analyzer: Analyzer) -> FullText {
    FullText {
        analyzer,
        ..FullText::default()
    }
}

/// A document whose text is `words`: one string, or under `pre_tokenized_array` one token
/// each.
fn doc(id: usize, words: &[&str], a: &Analyzer) -> Document {
    let mut d = Document::new(
        format!("d{id:05}"),
        (0..DIM).map(|j| ((id + j) % 7) as f32 / 7.0).collect(),
    );
    let v = if a.tokenizer == Tokenizer::PreTokenizedArray {
        Value::Array(words.iter().map(|w| Value::Str((*w).to_owned())).collect())
    } else {
        Value::Str(words.join(" "))
    };
    d.attrs.insert(FIELD.into(), v);
    d
}

async fn put<S: BlobStore>(store: &S, name: &str, docs: &[Document], a: &Analyzer) -> Target {
    let built = vec_index::try_build_all_fields(
        docs,
        Params {
            exact_scan_threshold: 10_000,
            ..Params::default()
        },
        pstore_format::DEFAULT_FIELD,
        None,
        &[(FIELD, a)],
        &[],
    )
    .unwrap();
    let key = Key::new(format!("t/idx/{name}.seg"));
    store.put(&key, built.segment).await.unwrap();
    store
        .put(
            &text::dict_key(&key),
            bytes::Bytes::from(built.text_dictionary.expect("a text dictionary")),
        )
        .await
        .unwrap();
    Target {
        segment_len: None,
        centroids: None,
        deleted: None,
        sparse_dict: false,
        text_dict: true,
        fde_clusters: false,
        shadowed: false,
        segment: key,
    }
}

fn leg(q: &str, last_as_prefix: bool) -> Prefetch {
    Prefetch::Text {
        field: FIELD.to_owned(),
        query: q.to_owned(),
        limit: 500,
        last_as_prefix,
    }
}

fn sum(w: &[f32]) -> Fusion {
    Fusion::Sum {
        weights: Weights::of(w).unwrap(),
    }
}

fn scores(hits: &[Hit]) -> BTreeMap<(usize, usize), f32> {
    hits.iter().map(|h| ((h.segment, h.row), h.score)).collect()
}

/// Segments of `words` rows each, under `a`; the leg's scores by (segment, row).
async fn run(
    a: &Analyzer,
    segments: &[&[&[&str]]],
    q: &str,
    last_as_prefix: bool,
) -> BTreeMap<(usize, usize), f32> {
    let s = MemoryStore::new();
    let mut targets = Vec::new();
    let mut id = 0;
    for (i, rows) in segments.iter().enumerate() {
        let docs: Vec<Document> = rows
            .iter()
            .map(|w| {
                id += 1;
                doc(id, w, a)
            })
            .collect();
        targets.push(put(&s, &format!("s{i}"), &docs, a).await);
    }
    scores(
        &query_with(
            &s,
            &targets,
            &[leg(q, last_as_prefix)],
            &fts(*a),
            sum(&[1.0]),
            500,
        )
        .await
        .unwrap(),
    )
}

/// `a` with neither stemming nor stopword removal: what makes the prefix's literal form.
fn plain(a: &Analyzer) -> Analyzer {
    Analyzer {
        stemming: false,
        remove_stopwords: false,
        ..*a
    }
}

/// The reference: BM25 over the terms before the last raw token, from global statistics, plus
/// 1.0 for a row holding any term that starts with either of the last token's forms.
fn reference(a: &Analyzer, segments: &[Vec<Vec<&str>>], q: &str) -> BTreeMap<(usize, usize), f32> {
    let full = fts(*a);
    let terms_of = |words: &[&str]| -> Vec<String> {
        if a.tokenizer == Tokenizer::PreTokenizedArray {
            words.iter().map(|w| (*w).to_owned()).collect()
        } else {
            text::analyze(a, &words.join(" "))
        }
    };
    let rows: Vec<Vec<Vec<String>>> = segments
        .iter()
        .map(|seg| seg.iter().map(|w| terms_of(w)).collect())
        .collect();
    let toks = text::query_tokens(a, q);
    let (scored, forms): (Vec<String>, Vec<String>) = match toks.split_last() {
        None => (text::analyze_query(a, q), Vec::new()),
        Some((last, before)) => {
            let raw = &q[last.span.clone()];
            let mut forms: Vec<String> = text::analyze(&plain(a), raw).into_iter().collect();
            forms.extend(last.term.clone());
            (
                before.iter().filter_map(|t| t.term.clone()).collect(),
                forms,
            )
        }
    };
    let scored: BTreeSet<String> = scored.into_iter().collect();
    let all: Vec<&Vec<String>> = rows.iter().flatten().collect();
    let n = all.len() as f32;
    let total: usize = all.iter().map(|r| r.len()).sum();
    let avgdl = total as f32 / n;
    let mut out = BTreeMap::new();
    for (si, seg) in rows.iter().enumerate() {
        for (ri, row) in seg.iter().enumerate() {
            let mut score = 0.0_f32;
            let mut hit = false;
            for t in &scored {
                let tf = row.iter().filter(|x| *x == t).count() as f32;
                if tf == 0.0 {
                    continue;
                }
                hit = true;
                let df = all.iter().filter(|r| r.contains(t)).count() as f32;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                let norm = full.k1 * (1.0 - full.b + full.b * row.len() as f32 / avgdl);
                score += idf * (tf * (full.k1 + 1.0)) / (tf + norm);
            }
            if row
                .iter()
                .any(|t| forms.iter().any(|f| t.starts_with(f.as_str())))
            {
                hit = true;
                score += 1.0;
            }
            if hit {
                out.insert((si, ri), score);
            }
        }
    }
    out
}

const VOCAB: [&str; 14] = [
    "walrus", "walnut", "wall", "wal", "lazy", "running", "run", "rune", "theory", "the", "Café",
    "cafe", "Wal", "foxes",
];

const QUERY_VOCAB: [&str; 14] = [
    "wal", "la", "lazy", "runn", "running", "the", "theo", "caf", "Café", "Wal", "fo", "zz", "r",
    "The",
];

/// Criterion 1: hits and scores equal the reference's, over three segments, five analyzers,
/// and queries of one to three raw tokens.
#[tokio::test]
async fn a_prefix_leg_matches_the_reference() {
    let segments: Vec<Vec<Vec<&str>>> = (0..3)
        .map(|s| {
            (0..12 + s * 3)
                .map(|r| {
                    let k = 2 + (r * 7 + s) % 4;
                    (0..k)
                        .map(|j| VOCAB[(r * 5 + j * 3 + s * 11 + r * j) % VOCAB.len()])
                        .collect()
                })
                .collect()
        })
        .collect();
    let views: Vec<Vec<&[&str]>> = segments
        .iter()
        .map(|seg| seg.iter().map(Vec::as_slice).collect())
        .collect();
    let views: Vec<&[&[&str]]> = views.iter().map(Vec::as_slice).collect();
    let mut queries: Vec<Vec<&str>> = Vec::new();
    for (i, a) in QUERY_VOCAB.iter().enumerate() {
        queries.push(vec![a]);
        queries.push(vec![QUERY_VOCAB[(i + 3) % QUERY_VOCAB.len()], a]);
        queries.push(vec![
            QUERY_VOCAB[(i + 5) % QUERY_VOCAB.len()],
            QUERY_VOCAB[(i + 8) % QUERY_VOCAB.len()],
            a,
        ]);
    }
    let mut prefixed = 0;
    for a in [
        Analyzer::default(),
        english(),
        case_sensitive(),
        folding(),
        pre_tokenized(),
    ] {
        for words in &queries {
            let q = if a.tokenizer == Tokenizer::PreTokenizedArray {
                words.join(&TOKEN_SEPARATOR.to_string())
            } else {
                words.join(" ")
            };
            let got = run(&a, &views, &q, true).await;
            let want = reference(&a, &segments, &q);
            assert_eq!(
                got.keys().collect::<Vec<_>>(),
                want.keys().collect::<Vec<_>>(),
                "{a:?} {q:?}"
            );
            for (k, w) in &want {
                assert!(
                    (got[k] - w).abs() < 1e-5,
                    "{a:?} {q:?} {k:?}: {} against {w}",
                    got[k]
                );
            }
            prefixed += usize::from(want.values().any(|v| v.fract() == 0.0 && *v >= 1.0));
        }
    }
    // The reference itself is not vacuous: many queries gain a prefix-only row.
    assert!(prefixed > 20, "{prefixed}");
}

/// Criterion 2: `wal` against `walrus` and `walnut`; the 1.0 once a row.
#[tokio::test]
async fn a_prefix_scores_one_once() {
    let d = Analyzer::default();
    let got = run(&d, &[&[&["walrus"], &["walnut"], &["fox"]]], "wal", true).await;
    assert_eq!(got, BTreeMap::from([((0, 0), 1.0), ((0, 1), 1.0)]));
    let got = run(&d, &[&[&["wal", "wal", "walnut"], &["fox"]]], "wal", true).await;
    assert_eq!(
        got,
        BTreeMap::from([((0, 0), 1.0)]),
        "a row's 1.0, not more"
    );
}

/// Criterion 2: the terms before the prefix are BM25's, the prefix adds 1.0 to them.
#[tokio::test]
async fn the_other_terms_are_bm25s() {
    let d = Analyzer::default();
    let rows: &[&[&str]] = &[&["lazy", "walrus"], &["lazy", "fox"], &["dog"]];
    let got = run(&d, &[rows], "lazy wal", true).await;
    let lazy = run(&d, &[rows], "lazy", false).await;
    assert_eq!(got.len(), 2);
    assert!(
        (got[&(0, 0)] - (lazy[&(0, 0)] + 1.0)).abs() < 1e-6,
        "{got:?}"
    );
    assert!((got[&(0, 1)] - lazy[&(0, 1)]).abs() < 1e-6, "{got:?}");
}

/// Criterion 2: under English stemming a partial word matches as typed and as stemmed, and
/// a complete word meets its stored stem.
#[tokio::test]
async fn a_prefix_matches_as_typed_and_as_stemmed() {
    let e = english();
    let rows: &[&[&str]] = &[&["running"], &["theory"], &["run", "fast"]];
    assert!(run(&e, &[rows], "runn", true).await.is_empty(), "runn");
    assert_eq!(
        run(&e, &[rows], "running", true).await,
        BTreeMap::from([((0, 0), 1.0), ((0, 2), 1.0)])
    );
    assert_eq!(
        run(&e, &[rows], "theory", true).await,
        BTreeMap::from([((0, 1), 1.0)])
    );
    // `the` is a stopword, so it has no stem form: its plain form meets `theori`.
    let rows: &[&[&str]] = &[&["theory"], &["the", "cat"], &["cat"]];
    assert_eq!(
        run(&e, &[rows], "the", true).await,
        BTreeMap::from([((0, 0), 1.0)])
    );
    // A row matching through both forms scores 1.0 once.
    assert!(
        text::analyze(&e, "theory theoryland")
            .iter()
            .any(|t| t.starts_with("theory")),
        "the fixture lost its plain-form term"
    );
    let rows: &[&[&str]] = &[&["theory", "theoryland"], &["cat"]];
    assert_eq!(
        run(&e, &[rows], "theory", true).await,
        BTreeMap::from([((0, 0), 1.0)])
    );
}

/// Criterion 2: case follows the analyzer; punctuation alone has no prefix.
#[tokio::test]
async fn a_prefix_is_folded_as_the_analyzer_folds() {
    let rows: &[&[&str]] = &[&["walrus"], &["fox"]];
    assert_eq!(
        run(&Analyzer::default(), &[rows], "Wal", true).await,
        BTreeMap::from([((0, 0), 1.0)])
    );
    assert!(
        run(&case_sensitive(), &[rows], "Wal", true)
            .await
            .is_empty()
    );
    // A stopword has no stem form, so only its plain form, folded, meets `theori`.
    let theory: &[&[&str]] = &[&["theory"], &["cat"]];
    assert_eq!(
        run(&english(), &[theory], "The", true).await,
        BTreeMap::from([((0, 0), 1.0)])
    );
    let d = Analyzer::default();
    assert_eq!(
        run(&d, &[rows], "?! ,", true).await,
        run(&d, &[rows], "?! ,", false).await
    );
}

/// Criterion 2: a segment holding none of the BM25 terms still walks the prefix.
#[tokio::test]
async fn a_prefix_only_segment_is_still_searched() {
    let d = Analyzer::default();
    let got = run(&d, &[&[&["lazy", "fox"]], &[&["walrus"]]], "lazy wal", true).await;
    assert_eq!(got.get(&(1, 0)), Some(&1.0), "{got:?}");
    assert!(got.contains_key(&(0, 0)));
}

/// Criterion 2: under `Sum` the prefix's 1.0 is weighted like any leg score.
#[tokio::test]
async fn a_prefix_is_weighted_in_a_sum() {
    let d = Analyzer::default();
    let s = MemoryStore::new();
    let docs = [doc(1, &["walrus"], &d), doc(2, &["fox"], &d)];
    let t = [put(&s, "w", &docs, &d).await];
    let got = scores(
        &query_with(
            &s,
            &t,
            &[leg("wal", true), leg("fox", false)],
            &fts(d),
            sum(&[2.0, 1.0]),
            10,
        )
        .await
        .unwrap(),
    );
    assert_eq!(got.get(&(0, 0)), Some(&2.0), "{got:?}");
}

/// Criterion 3: a cold prefix query takes the rounds the same query takes without it, and
/// at most two more GETs a segment, though the prefix matches ten terms.
#[tokio::test]
async fn a_prefix_leg_costs_no_round() {
    let d = Analyzer::default();
    let counting = DepthCounting::new(MemoryStore::with_coalesce_gap(0));
    let docs: Vec<Document> = (0..10)
        .map(|i| {
            let w = format!("pre{i}");
            doc(i, &["lazy", w.as_str(), "fox"], &d)
        })
        .collect();
    let t = [put(&counting, "c", &docs, &d).await];
    let mut cost = Vec::new();
    for prefix in [false, true] {
        counting.reset();
        let hits = query_with(
            &counting,
            &t,
            &[leg("lazy pre", prefix)],
            &fts(d),
            sum(&[1.0]),
            10,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 10);
        cost.push((counting.depth(), counting.requests()));
    }
    let [(d0, r0), (d1, r1)] = [cost[0], cost[1]];
    assert_eq!(d1, d0, "depth");
    assert!(r1 > r0 && r1 <= r0 + 2, "{r0} then {r1} requests");
}
