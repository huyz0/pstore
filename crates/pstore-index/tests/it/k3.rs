//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M260: BM25's `k3`, the query-term weight, against a scorer written from the formula.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, SegmentWriter, Value, text};
use pstore_index::text::{TextIndex, row_score};

const K1: f32 = 1.2;
const B: f32 = 0.75;

const BODIES: [&str; 6] = ["a b c", "a a d e f", "b b b", "c d e f g h", "a", "b c"];

fn docs() -> Vec<Document> {
    BODIES
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let mut d = Document::new(format!("d{i}"), vec![1.0, 0.0]);
            d.attrs.insert(
                text::DEFAULT_TEXT_FIELD.to_owned(),
                Value::Str((*body).to_owned()),
            );
            d
        })
        .collect()
}

async fn put(store: &MemoryStore, docs: &[Document]) -> Key {
    let built = text::build(docs, text::DEFAULT_TEXT_FIELD);
    let mut w = SegmentWriter::new(64);
    for d in docs {
        w.push(d.clone());
    }
    let key = Key::new("t/k3/text.seg");
    store
        .put(
            &key,
            w.with_section(Section::TextPostings, built.postings)
                .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
                .try_finish()
                .unwrap(),
        )
        .await
        .unwrap();
    store
        .put(&text::dict_key(&key), bytes::Bytes::from(built.dictionary))
        .await
        .unwrap();
    key
}

/// BM25 with `k3`, from the formula and sharing no code with the index: for each distinct
/// query term, in byte order, `term_score · w`, `w = ((k3 + 1) · qtf) / (k3 + qtf)`.
fn oracle(query: &[&str], k3: Option<f32>) -> Vec<f32> {
    let bodies: Vec<Vec<&str>> = BODIES.iter().map(|b| b.split(' ').collect()).collect();
    let n = bodies.len() as f32;
    let avgdl = bodies.iter().map(Vec::len).sum::<usize>() as f32 / n;
    let mut distinct: Vec<&str> = query.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    bodies
        .iter()
        .map(|body| {
            let mut score = 0.0_f32;
            for term in &distinct {
                let tf = body.iter().filter(|t| *t == term).count() as f32;
                if tf == 0.0 {
                    continue;
                }
                let df = bodies.iter().filter(|b| b.contains(term)).count() as f32;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                let norm = K1 * (1.0 - B + B * body.len() as f32 / avgdl);
                let qtf = query.iter().filter(|t| *t == term).count() as f32;
                let w = k3.map_or(1.0, |k3| ((k3 + 1.0) * qtf) / (k3 + qtf));
                score += idf * (tf * (K1 + 1.0)) / (tf + norm) * w;
            }
            score
        })
        .collect()
}

/// Every row's score under both of the index's paths: postings, and rescoring a row.
async fn both(query: &[&str], k3: Option<f32>) -> (Vec<f32>, Vec<f32>) {
    let docs = docs();
    let store = MemoryStore::with_coalesce_gap(256);
    let key = put(&store, &docs).await;
    let idx = TextIndex::open(&store, &key).await.unwrap();
    let stats = idx.summary();
    let terms: Vec<String> = query.iter().map(|t| (*t).to_owned()).collect();
    let hits = idx
        .search_prefixed(&store, &key, &terms, &[], &stats, 100, K1, B, k3)
        .await
        .unwrap();
    let mut postings = vec![0.0_f32; docs.len()];
    for (row, score) in hits {
        postings[row] = score;
    }
    let rows: Vec<f32> = BODIES
        .iter()
        .map(|b| {
            let row: Vec<String> = b.split(' ').map(str::to_owned).collect();
            row_score(&terms, &[], &row, &stats, K1, B, k3)
        })
        .collect();
    (postings, rows)
}

#[tokio::test]
async fn k3_weights_repeated_query_terms() {
    for k3 in [0.5_f32, 8.0, 1000.0] {
        let q = ["a", "a", "b"];
        let want = oracle(&q, Some(k3));
        let (postings, rows) = both(&q, Some(k3)).await;
        assert_eq!(postings, want, "postings under k3 {k3}");
        assert_eq!(rows, want, "row rescoring under k3 {k3}");
        // The weight is visible: `a` weighs more than once.
        assert_ne!(
            want,
            oracle(&["a", "b"], Some(k3)),
            "k3 {k3} changed nothing"
        );
    }
}

#[tokio::test]
async fn k3_counts_only_terms_not_prefixes() {
    // `"a a"` with `last_as_prefix` is the term `a` once and the prefix `a`: qtf 1, weight 1.
    let docs = docs();
    let store = MemoryStore::with_coalesce_gap(256);
    let key = put(&store, &docs).await;
    let idx = TextIndex::open(&store, &key).await.unwrap();
    let stats = idx.summary();
    let terms = vec!["a".to_owned()];
    let prefixes = vec!["a".to_owned()];
    let with = idx
        .search_prefixed(
            &store,
            &key,
            &terms,
            &prefixes,
            &stats,
            100,
            K1,
            B,
            Some(8.0),
        )
        .await
        .unwrap();
    let without = idx
        .search_prefixed(&store, &key, &terms, &prefixes, &stats, 100, K1, B, None)
        .await
        .unwrap();
    assert_eq!(with, without);
}

#[tokio::test]
async fn k3_unset_scores_each_term_once() {
    let repeated = both(&["a", "a", "b"], None).await;
    let once = both(&["a", "b"], None).await;
    assert_eq!(repeated, once);
    assert_eq!(repeated.0, oracle(&["a", "b"], None));
}

#[tokio::test]
async fn k3_without_repeats_is_bit_identical() {
    let q = ["a", "b", "c", "d"];
    let unset = both(&q, None).await;
    for k3 in [0.5_f32, 8.0, 1000.0] {
        let set = both(&q, Some(k3)).await;
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&set.0), bits(&unset.0), "postings under k3 {k3}");
        assert_eq!(bits(&set.1), bits(&unset.1), "rows under k3 {k3}");
    }
}
