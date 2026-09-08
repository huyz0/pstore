//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! BM25 over object storage, measured against an implementation that shares no code with it.
//!
//! ⚠️ **The oracle is the gate.** A judged set we generated ranks correctly under any
//! term-matching scorer, including one with `k1` and `b` transposed. A second implementation
//! of the formula, written from the formula, does not.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{Document, Section, SegmentWriter, Value, text};
use pstore_index::text::{Stats, TextIndex};
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;

const GAP: u64 = 256;
const K1: f32 = 1.2;
const B: f32 = 0.75;

fn doc(id: usize, body: &str) -> Document {
    let mut d = Document::new(format!("d{id:05}"), vec![1.0, 0.0]);
    d.attrs.insert(
        text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str(body.to_owned()),
    );
    d
}

/// A Zipf-skewed corpus: a uniform vocabulary makes every IDF the same and hides the term
/// that decides the ranking.
fn corpus(rows: usize, vocab: usize, len: usize, seed: u64) -> Vec<Document> {
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    (0..rows)
        .map(|i| {
            let body: Vec<String> = (0..len)
                .map(|_| {
                    #[expect(clippy::cast_precision_loss, reason = "a fixture, not a metric")]
                    let u = (next() % 1_000_000) as f64 / 1_000_000.0;
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "u*u*vocab is in range by construction"
                    )]
                    let t = ((u * u) * vocab as f64) as usize;
                    format!("t{t}")
                })
                .collect();
            doc(i, &body.join(" "))
        })
        .collect()
}

/// BM25, written from the formula, sharing no code with the index.
///
/// `score(d, Q) = Σ_t IDF(t) · tf·(k1+1) / (tf + k1·(1 − b + b·|d|/avgdl))`
/// with `IDF(t) = ln(1 + (N − df + 0.5)/(df + 0.5))`.
fn oracle(docs: &[Document], query: &[&str], k: usize) -> Vec<(usize, f32)> {
    let bodies: Vec<Vec<String>> = docs
        .iter()
        .map(|d| match d.attrs.get(text::DEFAULT_TEXT_FIELD) {
            Some(Value::Str(s)) => text::analyze(s),
            _ => Vec::new(),
        })
        .collect();
    let n = docs.len() as f32;
    let avgdl = bodies.iter().map(Vec::len).sum::<usize>() as f32 / n;
    // ⚠️ df computed ONCE per term, not once per (term, document). The naive placement is
    // inside the document loop, which makes the oracle O(N^2) -- 119 seconds on this fixture,
    // against a suite the whole project keeps under a few. A slow oracle gets deleted, and an
    // oracle that gets deleted is how a scorer stops being checked against anything.
    let idfs: Vec<f32> = query
        .iter()
        .map(|term| {
            let df = bodies
                .iter()
                .filter(|b| b.iter().any(|t| t.as_str() == *term))
                .count() as f32;
            (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    let mut out: Vec<(usize, f32)> = Vec::new();
    for (row, body) in bodies.iter().enumerate() {
        let mut score = 0.0f32;
        let mut hit = false;
        for (qi, term) in query.iter().enumerate() {
            let tf = body.iter().filter(|t| t.as_str() == *term).count() as f32;
            if tf == 0.0 {
                continue;
            }
            hit = true;
            let norm = K1 * (1.0 - B + B * body.len() as f32 / avgdl);
            score += idfs[qi] * (tf * (K1 + 1.0)) / (tf + norm);
        }
        if hit {
            out.push((row, score));
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(k);
    out
}

const K1_CONST: f32 = K1;

/// Writes `docs` as a segment with its term dictionary and fieldnorms.
async fn put<S: BlobStore>(store: &S, docs: &[Document], name: &str) -> Key {
    let built = text::build(docs, text::DEFAULT_TEXT_FIELD);
    let mut w = SegmentWriter::new(64);
    for d in docs {
        w.push(d.clone());
    }
    let key = Key::new(name);
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

/// 200 three-term queries, each drawn from a document of the corpus.
fn queries(docs: &[Document], n: usize) -> Vec<Vec<String>> {
    (0..n)
        .map(|i| {
            let d = &docs[i * docs.len() / n];
            let Some(Value::Str(s)) = d.attrs.get(text::DEFAULT_TEXT_FIELD) else {
                return Vec::new();
            };
            let mut terms = text::analyze(s);
            terms.sort();
            terms.dedup();
            terms.truncate(3);
            terms
        })
        .collect()
}

#[tokio::test]
async fn bm25_matches_an_independent_implementation() {
    // ⚠️ Criterion 4, and the real gate of this milestone. `k1`/`b` transposed, the `+1`
    // dropped from the IDF logarithm, or the length normalisation applied unnormalised all
    // produce a plausible ranking that no judged set of ours would reject.
    let docs = corpus(2_000, 3_000, 40, 0x51ee);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, "t/idx/text.seg").await;
    let idx = TextIndex::open(&store, &key).await.unwrap();
    let stats = idx.summary();

    let mut compared = 0usize;
    for q in queries(&docs, 200) {
        if q.is_empty() {
            continue;
        }
        let refs: Vec<&str> = q.iter().map(String::as_str).collect();
        let want = oracle(&docs, &refs, 10);
        let got = idx.search(&store, &key, &q, &stats, 10).await.unwrap();
        assert_eq!(
            got.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            want.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            "top-10 order differs from the oracle for {q:?}"
        );
        for ((_, a), (_, b)) in got.iter().zip(&want) {
            assert!((a - b).abs() <= 1e-4, "score {a} against the oracle's {b}");
        }
        compared += 1;
    }
    assert!(compared > 150, "only {compared} queries were comparable");
    assert_eq!(K1_CONST, 1.2);
}

#[tokio::test]
async fn a_text_query_costs_two_round_trips_beyond_head() {
    // ⚠️ Criterion 6, at the index layer. The dictionary rides beside the footer, and the
    // postings ride with the fieldnorms -- so global statistics cost no round of their own,
    // which is the whole of D-30's claim.
    let docs = corpus(20_000, 30_000, 120, 0xbeef);
    let inner = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&inner, &docs, "t/idx/gate.seg").await;
    let s = DepthCounting::new(inner);

    s.reset();
    let idx = TextIndex::open(&s, &key).await.unwrap();
    assert_eq!(s.depth(), 1, "opening cost {} rounds", s.depth());

    let stats = idx.summary();
    let q = queries(&docs, 1)[0].clone();
    s.reset();
    let hits = idx.search(&s, &key, &q, &stats, 10).await.unwrap();
    assert!(!hits.is_empty());
    assert_eq!(
        s.depth(),
        1,
        "the postings and fieldnorms cost {} rounds, so one waited on the other",
        s.depth()
    );
}

#[tokio::test]
async fn a_text_query_fetches_only_its_own_lists() {
    // ⚠️ Criterion 7. Reading the whole section and filtering in memory ranks identically and
    // moves the whole index over the wire.
    let docs = corpus(20_000, 30_000, 120, 0xbeef);
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let t = TenantId(31);
    let view = acct.as_tenant(t);
    let key = put(&view, &docs, "t/idx/gate.seg").await;
    let idx = TextIndex::open(&view, &key).await.unwrap();
    let stats = idx.summary();

    let q = queries(&docs, 7)[3].clone();
    let want = idx.list_bytes(&q);
    assert!(
        want > 0,
        "the query matched no list, so the bound is vacuous"
    );

    acct.record_ranges();
    idx.search(&view, &key, &q, &stats, 10).await.unwrap();
    let moved = acct.bytes_in(&key, idx.span());
    assert!(
        moved as f64 <= want as f64 * 1.2,
        "a {}-term query moved {moved} bytes of postings against {want} in its own lists",
        q.len()
    );
    assert_eq!(acct.count(t, OpClass::List), 0, "a text query listed");
}

#[tokio::test]
async fn an_unknown_term_costs_nothing() {
    // ⚠️ Criterion 8. A term the corpus never used is the common case, not an error.
    let docs = corpus(500, 400, 20, 7);
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let t = TenantId(32);
    let view = acct.as_tenant(t);
    let key = put(&view, &docs, "t/idx/small.seg").await;
    let idx = TextIndex::open(&view, &key).await.unwrap();
    let stats = idx.summary();

    let real = queries(&docs, 4)[1].clone();
    let mut padded = real.clone();
    padded.push("aardvark".to_owned());
    assert_eq!(
        idx.search(&view, &key, &real, &stats, 10).await.unwrap(),
        idx.search(&view, &key, &padded, &stats, 10).await.unwrap(),
        "an absent term changed the answer"
    );

    let before = acct.count(t, OpClass::Read);
    let none = idx
        .search(&view, &key, &["aardvark".to_owned()], &stats, 10)
        .await
        .expect("a query of only absent terms was an error");
    assert!(none.is_empty());
    assert_eq!(
        acct.count(t, OpClass::Read),
        before,
        "a query that can match nothing still went to the store"
    );
}

#[tokio::test]
async fn a_segment_with_no_text_is_not_a_text_index() {
    // A miss returning zero hits is indistinguishable from a corpus with no matches.
    let store = MemoryStore::with_coalesce_gap(GAP);
    let mut w = SegmentWriter::new(8);
    w.push(Document::new("plain", vec![1.0, 0.0]));
    let key = Key::new("t/idx/plain.seg");
    store.put(&key, w.try_finish().unwrap()).await.unwrap();
    assert!(TextIndex::open(&store, &key).await.is_err());
}

#[tokio::test]
async fn statistics_are_a_property_of_the_corpus_not_of_a_query() {
    // The summary D-30 gathers in RT-A: it comes from the dictionary alone, so `s` segments
    // cost `s` requests in the round that was happening anyway rather than a round of their
    // own.
    let docs = corpus(300, 200, 15, 3);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, "t/idx/s.seg").await;
    let idx = TextIndex::open(&store, &key).await.unwrap();
    let s = idx.summary();
    assert_eq!(s.doc_count, 300);
    assert_eq!(s.total_tokens, 300 * 15);
    let df = s.df.get("t0").copied().unwrap_or(0);
    let by_hand = docs
        .iter()
        .filter(|d| match d.attrs.get(text::DEFAULT_TEXT_FIELD) {
            Some(Value::Str(b)) => text::analyze(b).iter().any(|t| t == "t0"),
            _ => false,
        })
        .count() as u32;
    assert_eq!(df, by_hand, "the summary's df is not the corpus's");

    // Merging two summaries is addition, which is what makes two-pass IDF cheap.
    let merged = Stats::merge([s.clone(), s.clone()]);
    assert_eq!(merged.doc_count, 600);
    assert_eq!(merged.total_tokens, 300 * 15 * 2);
    assert_eq!(merged.df.get("t0"), Some(&(by_hand * 2)));
}

// ---------------------------------------------------------------------------
// M5c.3 — two-pass IDF (D-30), and OQ-64's number.
// ---------------------------------------------------------------------------

/// The pinned two-segment fixture: `common` is in 900 of segment A's 1,000 documents and 10
/// of segment B's; `rare` is in 200 of B's and none of A's.
///
/// ⚠️ The ratio is chosen so the disagreement reaches the **top-1**, not merely the score. An
/// IDF difference that leaves the order unchanged makes "ranks differently" vacuous.
fn two_segments() -> (Vec<Document>, Vec<Document>) {
    let filler = |i: usize| format!("f{} f{} f{}", i % 37, i % 41, i % 43);
    let a: Vec<Document> = (0..1_000)
        .map(|i| {
            let body = if i < 900 {
                format!("common {}", filler(i))
            } else {
                filler(i)
            };
            doc(i, &body)
        })
        .collect();
    let b: Vec<Document> = (0..1_000)
        .map(|i| {
            let body = if i < 10 {
                format!("common {}", filler(i))
            } else if i < 210 {
                format!("rare {}", filler(i))
            } else {
                filler(i)
            };
            doc(10_000 + i, &body)
        })
        .collect();
    (a, b)
}

#[tokio::test]
async fn global_idf_changes_the_top_result() {
    // ⚠️ Criterion 5, and the correctness bug D-30 calls "easy to ship wrong and hard to
    // notice". Scoring segment B with B's OWN document frequencies makes `common` look rare —
    // it is, in B — and a document containing it outranks one containing `rare`. Over the two
    // segments together the truth is the other way round. Both answers are internally
    // consistent; only one is right.
    let (a, b) = two_segments();
    let store = MemoryStore::with_coalesce_gap(GAP);
    let ka = put(&store, &a, "t/idx/seg-a").await;
    let kb = put(&store, &b, "t/idx/seg-b").await;
    let ia = TextIndex::open(&store, &ka).await.unwrap();
    let ib = TextIndex::open(&store, &kb).await.unwrap();

    let global = Stats::merge([ia.summary(), ib.summary()]);
    assert_eq!(global.doc_count, 2_000);
    assert_eq!(global.df.get("common"), Some(&910));
    assert_eq!(global.df.get("rare"), Some(&200));

    let per_segment = ib.summary();
    assert_eq!(per_segment.df.get("common"), Some(&10));

    let q = vec!["common".to_owned(), "rare".to_owned()];
    let with_global = ib.search(&store, &kb, &q, &global, 5).await.unwrap();
    let with_local = ib.search(&store, &kb, &q, &per_segment, 5).await.unwrap();
    assert_ne!(
        with_global[0].0, with_local[0].0,
        "per-segment and global IDF agreed on the top result, so the fixture proves nothing"
    );

    // The global answer is the one an oracle over the UNION gives.
    let union: Vec<Document> = a.iter().chain(&b).cloned().collect();
    let truth = oracle(&union, &["common", "rare"], union.len());
    let best_in_b = truth
        .iter()
        .find(|(row, _)| *row >= a.len())
        .map(|(row, _)| row - a.len())
        .expect("the oracle ranked nothing from segment B");
    assert_eq!(
        with_global[0].0, best_in_b,
        "global IDF did not reproduce the union's ranking"
    );
    assert_ne!(
        with_local[0].0, best_in_b,
        "the fixture's local answer happens to be right, so it tests nothing"
    );
}

#[tokio::test]
async fn oq64_how_often_per_segment_idf_is_wrong() {
    // ⚠️ OQ-64: "quantify the ranking error from per-shard IDF". A number, not an adjective —
    // and deliberately measured on a fixture whose segments are **unlike each other**, because
    // `hybrid-and-ranking.md` option (2) argues per-shard IDF is fine when hash-by-id sharding
    // makes shards statistically similar. That argument is exactly what this measures the
    // limit of.
    let (a, b) = two_segments();
    let store = MemoryStore::with_coalesce_gap(GAP);
    let ka = put(&store, &a, "t/idx/seg-a").await;
    let kb = put(&store, &b, "t/idx/seg-b").await;
    let ia = TextIndex::open(&store, &ka).await.unwrap();
    let ib = TextIndex::open(&store, &kb).await.unwrap();
    let global = Stats::merge([ia.summary(), ib.summary()]);
    let local = ib.summary();

    // ⚠️ Two terms, one of which has a divergent df and one of which does not. A third,
    // very common term was tried first and made every query agree — it dominated both
    // scorers, which is a real property worth knowing: per-segment IDF is wrong exactly when
    // the query's *discriminating* term is the one whose frequency differs between segments.
    let mut differ = 0usize;
    let mut total = 0usize;
    for i in 0..37usize {
        let q = vec!["common".to_owned(), format!("f{i}")];
        let g = ib.search(&store, &kb, &q, &global, 10).await.unwrap();
        let l = ib.search(&store, &kb, &q, &local, 10).await.unwrap();
        if g.is_empty() || l.is_empty() {
            continue;
        }
        total += 1;
        if g[0].0 != l[0].0 {
            differ += 1;
        }
    }
    assert!(total >= 30, "only {total} queries were comparable");
    println!("OQ-64 per-segment vs global IDF: top-1 differs on {differ}/{total} queries");
    assert!(
        differ > 0,
        "per-segment IDF never changed the top result on a fixture built to make it"
    );
}

#[test]
fn an_empty_corpus_has_no_average_document_length() {
    // ⚠️ A division that would be by zero. BM25's norm degenerates to `k1·(1−b)`, which is a
    // defined score rather than a NaN that sorts unpredictably against every other row.
    let empty = Stats::default();
    assert_eq!(empty.avgdl(), 0.0);
    let one = Stats {
        doc_count: 4,
        total_tokens: 40,
        df: std::collections::BTreeMap::new(),
    };
    assert_eq!(one.avgdl(), 10.0);
    assert_eq!(Stats::merge([]), Stats::default());
}

#[tokio::test]
async fn the_opened_index_reports_the_vocabulary_it_scores_from() {
    // The dictionary a query is answered from, exposed so a caller can see the vocabulary
    // rather than infer it from empty results — and so `list_bytes` has something to be
    // checked against.
    let docs = corpus(400, 200, 15, 11);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, "t/idx/v.seg").await;
    let idx = TextIndex::open(&store, &key).await.unwrap();
    let dict = idx.dictionary();
    assert!(!dict.is_empty());
    assert_eq!(dict.doc_count(), 400);

    let q = queries(&docs, 4)[0].clone();
    let by_hand: u64 = q
        .iter()
        .filter_map(|t| dict.lookup(t))
        .map(|e| u64::from(e.bytes))
        .sum();
    assert_eq!(idx.list_bytes(&q), by_hand);
    assert_eq!(idx.list_bytes(&["aardvark".to_owned()]), 0);
}
