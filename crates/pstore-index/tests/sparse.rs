//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Exact sparse retrieval: no probing, no recall knob, and one round of postings.
//!
//! ⚠️ **"Exact" is a claim about candidates, not about scores.** Every row carrying any query
//! dimension is scored, which is what makes this a smaller subsystem than dense ANN. The
//! impacts are quantized, so ranking is exact only against an f32 oracle — the two are pinned
//! by different tests on purpose, because collapsing them makes one of them vacuous.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::sparse::{self, ImpactEncoding};
use pstore_format::{Document, Impact, Section, SegmentWriter, VectorField};
use pstore_index::sparse::SparseIndex;
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;
use std::collections::BTreeMap;

const FIELD: &str = "body_sparse";
/// ⚠️ Pinned by the spec, and it decides the result: at the 64 KiB default the coalescer
/// merges most of a postings section, so "only the query's own lists" would measure the
/// whole-section read it exists to refuse.
const GAP: u64 = 256;

fn doc(id: usize, pairs: &[(u32, f32)]) -> Document {
    Document {
        id: format!("d{id:05}"),
        vectors: BTreeMap::from([(
            FIELD.to_owned(),
            VectorField::Sparse(pairs.iter().map(|(d, w)| (*d, Impact::new(*w))).collect()),
        )]),
        attrs: BTreeMap::new(),
    }
}

/// A corpus whose dimension frequencies are **skewed**, which is what a real vocabulary is.
///
/// ⚠️ A uniform draw makes every posting list the same length and hides the cost the byte
/// bound exists to catch: one hot term whose list is a large fraction of the section.
fn corpus(rows: usize, vocab: u32, nnz: usize) -> Vec<Document> {
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    (0..rows)
        .map(|i| {
            let mut pairs: Vec<(u32, f32)> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            while seen.len() < nnz {
                // Zipf-ish: square the uniform draw so low dimensions dominate.
                #[expect(clippy::cast_precision_loss, reason = "a fixture, not a metric")]
                let u = (next() % 1_000_000) as f64 / 1_000_000.0;
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "u*u*vocab is in range by construction"
                )]
                let d = ((u * u) * f64::from(vocab)) as u32;
                if seen.insert(d) {
                    #[expect(clippy::cast_precision_loss, reason = "a fixture weight")]
                    let w = (next() % 900) as f32 / 1000.0 + 0.1;
                    pairs.push((d, w));
                }
            }
            pairs.sort_by_key(|(d, _)| *d);
            doc(i, &pairs)
        })
        .collect()
}

/// Writes `docs` as a segment with its dictionary beside it.
async fn put<S: BlobStore>(store: &S, docs: &[Document], enc: ImpactEncoding) -> Key {
    let p = sparse::build(docs, FIELD, enc);
    let mut w = SegmentWriter::new(64);
    for d in docs {
        w.push(d.clone());
    }
    let key = Key::new("t/idx/sparse.seg");
    store
        .put(
            &key,
            w.with_section(Section::SparsePostings, p.section)
                .try_finish()
                .unwrap(),
        )
        .await
        .unwrap();
    store
        .put(&sparse::dict_key(&key), bytes::Bytes::from(p.dictionary))
        .await
        .unwrap();
    key
}

/// Every row sharing a dimension with `q`, and its exact f32 score.
fn brute(docs: &[Document], q: &[(u32, f32)]) -> Vec<(usize, f32)> {
    let mut out: Vec<(usize, f32)> = Vec::new();
    for (row, d) in docs.iter().enumerate() {
        let VectorField::Sparse(pairs) = &d.vectors[FIELD] else {
            continue;
        };
        let mut score = 0.0f32;
        let mut hit = false;
        for (qd, qw) in q {
            if let Some((_, w)) = pairs.iter().find(|(x, _)| x == qd) {
                score += qw * w.get();
                hit = true;
            }
        }
        if hit {
            out.push((row, score));
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// 200 queries, each the sparse field of a document of the corpus — the pinned distribution,
/// because rank agreement under quantization is far more forgiving for uniformly drawn
/// dimensions than for frequency-drawn ones.
fn queries(docs: &[Document], n: usize) -> Vec<Vec<(u32, f32)>> {
    (0..n)
        .map(|i| {
            let d = &docs[i * docs.len() / n];
            match &d.vectors[FIELD] {
                VectorField::Sparse(p) => p.iter().map(|(x, w)| (*x, w.get())).collect(),
                VectorField::Dense(_) => Vec::new(),
            }
        })
        .collect()
}

#[tokio::test]
async fn the_candidate_set_is_exact() {
    // ⚠️ Criterion 7, as a SET. Sparse search has no recall knob: a row carrying any query
    // dimension is scored, full stop. A list truncated by one, or a query term quietly
    // dropped, loses documents that no ranking check would notice — the scores of the rows
    // that survived are all still right.
    let docs = corpus(2_000, 3_000, 8);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, ImpactEncoding::F32).await;
    let idx = SparseIndex::open(&store, &key, FIELD).await.unwrap();

    for q in queries(&docs, 200) {
        let want: std::collections::BTreeSet<usize> =
            brute(&docs, &q).into_iter().map(|(r, _)| r).collect();
        let got: std::collections::BTreeSet<usize> = idx
            .search(&store, &key, &q, docs.len())
            .await
            .unwrap()
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(got, want, "the candidate set is not exact");
    }
}

#[tokio::test]
async fn f32_impacts_rank_exactly_like_brute_force() {
    // ⚠️ Criterion 8: the search logic with quantization removed. Anything wrong in the
    // accumulator -- `min` for `+=`, the query's weight dropped, the wrong row -- shows up
    // here as a different order, and nowhere else as anything at all.
    let docs = corpus(2_000, 3_000, 8);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, ImpactEncoding::F32).await;
    let idx = SparseIndex::open(&store, &key, FIELD).await.unwrap();

    for q in queries(&docs, 200) {
        let want: Vec<(usize, f32)> = brute(&docs, &q).into_iter().take(10).collect();
        let got = idx.search(&store, &key, &q, 10).await.unwrap();
        assert_eq!(
            got.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            want.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            "the top-10 order differs from brute force"
        );
        for ((_, a), (_, b)) in got.iter().zip(&want) {
            assert!((a - b).abs() <= 1e-4, "score {a} against {b}");
        }
    }
}

/// The gate corpus: 20,000 rows over a SPLADE-sized 30,000-dimension vocabulary.
///
/// ⚠️ Pinned by the spec, and the size the whole sidecar deviation exists for: a fixture with
/// a 50-term vocabulary would open in one round no matter where the dictionary lived.
fn gate_corpus() -> Vec<Document> {
    corpus(20_000, 30_000, 32)
}

#[tokio::test]
async fn a_sparse_query_costs_two_round_trips_beyond_head() {
    // ⚠️ Criterion 9, at the index layer -- HEAD is the engine's round, which makes a cold
    // query three. Equality, not a bound: a query that skipped the dictionary would measure
    // FEWER rounds and pass a `<=`, while answering from a vocabulary it never read.
    let docs = gate_corpus();
    let inner = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&inner, &docs, ImpactEncoding::U8).await;
    let s = DepthCounting::new(inner);

    s.reset();
    let idx = SparseIndex::open(&s, &key, FIELD).await.unwrap();
    assert_eq!(
        s.depth(),
        1,
        "opening cost {} rounds: the dictionary did not ride beside the footer",
        s.depth()
    );

    s.reset();
    let hits = idx
        .search(&s, &key, &queries(&docs, 1)[0], 10)
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(
        s.depth(),
        1,
        "the postings fetch cost {} rounds, so the lists went out in a loop",
        s.depth()
    );
}

#[tokio::test]
async fn the_postings_fetch_is_one_round_whatever_the_term_count() {
    // ⚠️ Criterion 10. Depth, not request count: `get_ranges` coalesces and then issues one
    // request per coalesced span, so a 32-term query is up to 32 REQUESTS -- in one round.
    // Requests scaling with query terms is allowed; depth is not.
    let docs = gate_corpus();
    let inner = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&inner, &docs, ImpactEncoding::U8).await;
    let s = DepthCounting::new(inner);
    let idx = SparseIndex::open(&s, &key, FIELD).await.unwrap();

    // A deliberately wide query: every dimension the corpus uses.
    let mut wide: Vec<(u32, f32)> = Vec::new();
    for q in queries(&docs, 200) {
        wide.extend(q);
    }
    wide.sort_by_key(|(d, _)| *d);
    wide.dedup_by_key(|(d, _)| *d);
    assert!(wide.len() > 100, "the fixture is not wide enough to matter");

    s.reset();
    idx.search(&s, &key, &wide, 10).await.unwrap();
    assert_eq!(
        s.depth(),
        1,
        "a {}-term query cost {} rounds",
        wide.len(),
        s.depth()
    );
}

#[tokio::test]
async fn a_query_fetches_only_its_own_lists() {
    // ⚠️ Criterion 11. The failure this catches STILL RETURNS THE RIGHT ANSWER: reading the
    // whole section and filtering in memory ranks identically and moves the whole index over
    // the wire. Scoped to bytes inside the postings span, because the dictionary sidecar is
    // two orders of magnitude larger than a few lists and would swamp any unscoped bound.
    let docs = gate_corpus();
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let t = TenantId(9);
    let view = acct.as_tenant(t);
    let key = put(&view, &docs, ImpactEncoding::U8).await;
    let idx = SparseIndex::open(&view, &key, FIELD).await.unwrap();

    let q = &queries(&docs, 200)[3];
    let want: u64 = idx.list_bytes(q);
    assert!(
        want > 0,
        "the query matched no list, so the bound is vacuous"
    );

    acct.record_ranges();
    let before = acct.bytes_in(&key, idx.span());
    idx.search(&view, &key, q, 10).await.unwrap();
    let moved = acct.bytes_in(&key, idx.span()) - before;
    assert!(
        moved as f64 <= want as f64 * 1.2,
        "a {}-term query moved {moved} bytes of postings against {want} in its own lists",
        q.len()
    );
    assert_eq!(acct.count(t, OpClass::List), 0, "a sparse query listed");
}

#[tokio::test]
async fn an_unknown_dimension_costs_nothing() {
    // ⚠️ Criterion 12. A term the corpus never used is the COMMON case, not an error: it
    // contributes nothing and must cost nothing. A `0..0` range issued anyway is invisible to
    // a request counter -- `coalesce` drops empty ranges -- so what is asserted is the
    // observable half: no error, no bytes, and an all-absent query that returns nothing
    // rather than failing.
    let docs = corpus(500, 200, 4);
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let t = TenantId(10);
    let view = acct.as_tenant(t);
    let key = put(&view, &docs, ImpactEncoding::U8).await;
    let idx = SparseIndex::open(&view, &key, FIELD).await.unwrap();

    let real = &queries(&docs, 4)[1];
    let mut padded = real.clone();
    padded.push((900_000, 1.0));
    let plain = idx.search(&view, &key, real, 10).await.unwrap();
    let with_absent = idx.search(&view, &key, &padded, 10).await.unwrap();
    assert_eq!(plain, with_absent, "an absent dimension changed the answer");

    let before = acct.count(t, OpClass::Read);
    let none = idx
        .search(&view, &key, &[(900_000, 1.0), (900_001, 2.0)], 10)
        .await
        .expect("a query of only absent dimensions was an error");
    assert!(none.is_empty(), "absent dimensions produced hits");
    assert_eq!(
        acct.count(t, OpClass::Read),
        before,
        "a query that can match nothing still went to the store"
    );
}

#[tokio::test]
async fn a_field_that_is_not_sparse_is_an_error_not_an_empty_answer() {
    // A miss returning zero hits is indistinguishable from a field with no matches, so a
    // caller could not tell a typo from data. The same rule `VecIndex::search_field` follows.
    let docs = corpus(50, 100, 3);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, ImpactEncoding::U8).await;
    assert!(
        SparseIndex::open(&store, &key, "no_such_field")
            .await
            .is_err()
    );
}

/// One encoding's answers to the pinned query set.
type Answers = Vec<Vec<(usize, f32)>>;

/// Mean top-`k` set overlap and top-1 agreement of `got` against `want`.
fn agreement(got: &Answers, want: &Answers, k: usize) -> (f64, f64) {
    let mut overlap = 0.0f64;
    let mut top1 = 0.0f64;
    for (g, w) in got.iter().zip(want) {
        let gs: std::collections::BTreeSet<usize> = g.iter().take(k).map(|(r, _)| *r).collect();
        let ws: std::collections::BTreeSet<usize> = w.iter().take(k).map(|(r, _)| *r).collect();
        let denom = ws.len().max(1) as f64;
        overlap += gs.intersection(&ws).count() as f64 / denom;
        if g.first().map(|(r, _)| *r) == w.first().map(|(r, _)| *r) {
            top1 += 1.0;
        }
    }
    let n = got.len().max(1) as f64;
    (overlap / n, top1 / n)
}

#[tokio::test]
async fn impact_encodings_are_measured() {
    // ⚠️ Criterion 13, and it answers **both halves** of OQ-126: bytes, and what the cheaper
    // bytes cost in ranking. Measuring only agreement answers a question OQ-126 does not ask
    // and skips the one it does.
    //
    // ⚠️ The floor is pinned in the spec, ahead of this measurement. If u8 misses it, what
    // changes is `sparse::DEFAULT_ENCODING` -- never the floor.
    const OVERLAP_FLOOR: f64 = 0.95;
    const TOP1_FLOOR: f64 = 0.90;

    // Bytes on the gate corpus, where a section is worth measuring.
    let gate = gate_corpus();
    let mut bytes = Vec::new();
    for enc in [ImpactEncoding::U8, ImpactEncoding::F16, ImpactEncoding::F32] {
        let p = sparse::build(&gate, FIELD, enc);
        bytes.push((enc, p.section.len(), p.dictionary.len()));
    }
    for (enc, section, dict) in &bytes {
        println!("OQ-126 bytes  {enc:?}: postings {section}, dictionary {dict}");
    }
    let u8_bytes = bytes[0].1 as f64;
    let f32_bytes = bytes[2].1 as f64;
    assert!(
        u8_bytes < f32_bytes,
        "u8 impacts did not shrink the section: {u8_bytes} against {f32_bytes}"
    );

    // Agreement on the exactness corpus, against f32 as the oracle, over the pinned queries.
    let docs = corpus(2_000, 3_000, 8);
    let qs = queries(&docs, 200);
    let store = MemoryStore::with_coalesce_gap(GAP);

    let mut answers: Vec<(ImpactEncoding, Answers)> = Vec::new();
    for enc in [ImpactEncoding::F32, ImpactEncoding::U8, ImpactEncoding::F16] {
        let key = put(&store, &docs, enc).await;
        let idx = SparseIndex::open(&store, &key, FIELD).await.unwrap();
        let mut out = Vec::with_capacity(qs.len());
        for q in &qs {
            out.push(idx.search(&store, &key, q, 10).await.unwrap());
        }
        answers.push((enc, out));
    }
    let oracle = answers[0].1.clone();
    let mut measured = Vec::new();
    for (enc, got) in answers.iter().skip(1) {
        let (overlap, top1) = agreement(got, &oracle, 10);
        println!("OQ-126 rank   {enc:?}: top-10 overlap {overlap:.4}, top-1 {top1:.4}");
        measured.push((*enc, overlap, top1));
    }

    let (_, u8_overlap, u8_top1) = measured[0];
    assert!(
        u8_overlap >= OVERLAP_FLOOR && u8_top1 >= TOP1_FLOOR,
        "u8 measured overlap {u8_overlap:.4} / top-1 {u8_top1:.4} against the pinned floor \
         {OVERLAP_FLOOR} / {TOP1_FLOOR}: the DEFAULT ENCODING must change to f16, not the floor"
    );
    // The default is what the measurement selected, not what was assumed.
    assert_eq!(
        sparse::DEFAULT_ENCODING,
        ImpactEncoding::U8,
        "the shipped default no longer matches the encoding this measurement clears"
    );
}

#[tokio::test]
async fn a_dense_field_asked_for_sparsely_is_an_error() {
    // ⚠️ Not zero hits. A dense field opened as sparse would address the dense vectors as a
    // postings section and score whatever they decoded to -- and returning nothing instead
    // reads exactly like a sparse field with no matches, which a caller cannot tell from a
    // typo in a field name.
    let mut docs = corpus(50, 100, 3);
    for (i, d) in docs.iter_mut().enumerate() {
        d.vectors
            .insert("dense".to_owned(), VectorField::dense(vec![i as f32, 1.0]));
    }
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, ImpactEncoding::U8).await;
    assert!(
        SparseIndex::open(&store, &key, "dense").await.is_err(),
        "a dense field opened as a sparse one"
    );
    assert!(SparseIndex::open(&store, &key, FIELD).await.is_ok());
}

#[tokio::test]
async fn a_missing_dictionary_is_an_error_not_an_empty_vocabulary() {
    // ⚠️ Unlike the centroid table, whose absence MEANS something (D-10: scan exactly), a
    // missing dictionary leaves the postings an undelimited byte string. An empty vocabulary
    // would answer every query with nothing, which is a correct-looking answer.
    let docs = corpus(50, 100, 3);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, ImpactEncoding::U8).await;
    store.delete_batch(&[sparse::dict_key(&key)]).await.unwrap();
    assert!(SparseIndex::open(&store, &key, FIELD).await.is_err());
}

#[tokio::test]
async fn the_opened_index_reports_the_vocabulary_it_will_search() {
    // The dictionary a query is answered from, exposed so a caller can see the vocabulary
    // rather than infer it from empty results -- and so `list_bytes` has something to be
    // checked against.
    let docs = corpus(500, 200, 4);
    let store = MemoryStore::with_coalesce_gap(GAP);
    let key = put(&store, &docs, ImpactEncoding::U8).await;
    let idx = SparseIndex::open(&store, &key, FIELD).await.unwrap();
    let dict = idx.dictionary();
    assert!(!dict.is_empty());
    assert_eq!(dict.encoding(), ImpactEncoding::U8);
    let q = &queries(&docs, 4)[0];
    let by_hand: u64 = q
        .iter()
        .filter_map(|(d, _)| dict.lookup(*d))
        .map(|e| u64::from(e.bytes))
        .sum();
    assert_eq!(idx.list_bytes(q), by_hand);
    assert_eq!(idx.list_bytes(&[(900_000, 1.0)]), 0);
}
