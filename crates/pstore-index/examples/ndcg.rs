//! The ranking-quality gate (D-31), with a control that makes it able to fail.
//!
//! ⚠️ **A judged set we generated is not a quality measurement.** Documents judged relevant
//! because they contain the query's terms are ranked first by *any* term-matching scorer,
//! including one with `k1` and `b` transposed or IDF removed entirely. `gate-design` names
//! that shape exactly: a check that cannot fail reports success while checking nothing.
//!
//! So this runs **two** scorers over the same judged set — the shipped one, and a control
//! with IDF removed — and fails if the control clears the floor. What the gate measures is
//! therefore not "is BM25 good" but "does this corpus discriminate, and is the shipped scorer
//! on the right side of it". Absolute quality is criterion 4's job: agreement with an
//! independent implementation of the formula.
//!
//!   scripts/ndcg.sh

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, SegmentWriter, Value, text};
use pstore_index::text::{Stats, TextIndex};

/// Documents in the judged corpus.
const DOCS: usize = 2_000;
/// Judged queries.
const QUERIES: usize = 60;
/// Documents planted with each query's marker term — its relevant set.
const RELEVANT: usize = 5;
/// The floor the shipped scorer must clear.
const NDCG_FLOOR: f64 = 0.80;
/// The floor the shipped scorer must clear on mean reciprocal rank.
const MRR_FLOOR: f64 = 0.75;

fn main() {
    let (docs, queries) = judged();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (ndcg, mrr) = rt.block_on(measure(&docs, &queries, true));
    let (c_ndcg, c_mrr) = rt.block_on(measure(&docs, &queries, false));

    println!("corpus       {DOCS} documents, {QUERIES} queries, {RELEVANT} relevant each");
    println!("bm25         NDCG@10 {ndcg:.4}  MRR {mrr:.4}");
    println!("control      NDCG@10 {c_ndcg:.4}  MRR {c_mrr:.4}   (IDF removed)");
    println!("floors       NDCG@10 {NDCG_FLOOR:.2}  MRR {MRR_FLOOR:.2}");

    let mut bad = false;
    if ndcg < NDCG_FLOOR || mrr < MRR_FLOOR {
        eprintln!("FAIL: the shipped scorer is below the floor");
        bad = true;
    }
    // ⚠️ The half that makes the gate a gate.
    if c_ndcg >= NDCG_FLOOR && c_mrr >= MRR_FLOOR {
        eprintln!(
            "FAIL: the control cleared the floor too, so this corpus cannot tell a ranker \
             from a term matcher and the number above means nothing"
        );
        bad = true;
    }
    if bad {
        std::process::exit(1);
    }
    println!("ok: the shipped scorer clears the floor and the control does not");
}

/// One query: its terms, and the rows judged relevant to it.
type Judged = (Vec<String>, Vec<usize>);

/// A corpus whose relevance judgments come from **planting**, not from term overlap: each
/// query owns a marker term that appears in exactly [`RELEVANT`] documents.
fn judged() -> (Vec<Document>, Vec<Judged>) {
    let mut rng: u64 = 0x0ddc_0ffe_ebad_f00d;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut bodies: Vec<Vec<String>> = (0..DOCS)
        .map(|_| {
            (0..40)
                .map(|_| {
                    let u = (next() % 1_000_000) as f64 / 1_000_000.0;
                    format!("t{}", ((u * u) * 200.0) as usize)
                })
                .collect()
        })
        .collect();

    let mut queries = Vec::with_capacity(QUERIES);
    for q in 0..QUERIES {
        let marker = format!("m{q}");
        let mut relevant = Vec::with_capacity(RELEVANT);
        for r in 0..RELEVANT {
            let row = (q * 31 + r * 397) % DOCS;
            bodies[row].push(marker.clone());
            relevant.push(row);
        }
        relevant.sort_unstable();
        // Two common terms beside the marker, so a scorer that ignores IDF has something
        // else to be misled by.
        let terms = vec![marker, "t0".to_owned(), "t1".to_owned()];
        queries.push((terms, relevant));
    }

    let docs = bodies
        .into_iter()
        .enumerate()
        .map(|(i, body)| {
            let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.0]);
            d.attrs.insert(
                text::DEFAULT_TEXT_FIELD.to_owned(),
                Value::Str(body.join(" ")),
            );
            d
        })
        .collect();
    (docs, queries)
}

/// NDCG@10 and MRR over the judged set. `idf` false is the control.
async fn measure(docs: &[Document], queries: &[Judged], idf: bool) -> (f64, f64) {
    let store = MemoryStore::new();
    let built = text::build(docs, text::DEFAULT_TEXT_FIELD);
    let mut w = SegmentWriter::new(256);
    for d in docs {
        w.push(d.clone());
    }
    let key = Key::new("ndcg/seg");
    store
        .put(
            &key,
            w.with_section(Section::TextPostings, built.postings)
                .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
                .try_finish()
                .expect("segment"),
        )
        .await
        .expect("put");
    store
        .put(&text::dict_key(&key), bytes::Bytes::from(built.dictionary))
        .await
        .expect("put dict");
    let idx = TextIndex::open(&store, &key).await.expect("open");

    let real = idx.summary();
    // ⚠️ The control: every term's df set to the same value, which makes every IDF equal and
    // turns BM25 into length-normalised term counting. It is the single most consequential
    // thing a scorer can get wrong while still returning plausible results.
    let flat = Stats {
        doc_count: real.doc_count,
        total_tokens: real.total_tokens,
        df: real.df.keys().map(|t| (t.clone(), 1)).collect(),
    };
    let stats = if idf { &real } else { &flat };

    let mut ndcg_sum = 0.0f64;
    let mut rr_sum = 0.0f64;
    for (terms, relevant) in queries {
        let hits = idx
            .search(&store, &key, terms, stats, 10)
            .await
            .expect("search");
        let mut dcg = 0.0f64;
        let mut rr = 0.0f64;
        for (i, (row, _)) in hits.iter().enumerate() {
            if relevant.binary_search(row).is_ok() {
                dcg += 1.0 / ((i + 2) as f64).log2();
                if rr == 0.0 {
                    rr = 1.0 / (i + 1) as f64;
                }
            }
        }
        let ideal: f64 = (0..relevant.len().min(10))
            .map(|i| 1.0 / ((i + 2) as f64).log2())
            .sum();
        ndcg_sum += if ideal > 0.0 { dcg / ideal } else { 0.0 };
        rr_sum += rr;
    }
    let n = queries.len() as f64;
    (ndcg_sum / n, rr_sum / n)
}
