//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The sparse posting layout: a dictionary keyed by dimension, and one contiguous byte range
//! per term.
//!
//! ⚠️ **The dictionary is a sibling object, not the index section.** `INDEX_BUDGET` is 8,150
//! bytes and `try_finish` *refuses* a segment that exceeds it, so a 30,000-term vocabulary in
//! there does not make the open slow — it makes the segment unwritable. C-10.

use pstore_format::sparse::{self, Dictionary, ImpactEncoding};
use pstore_format::{Document, Impact, VectorField};
use std::collections::BTreeMap;

const FIELD: &str = "body_sparse";

/// A document whose sparse field is `pairs`.
fn doc(id: usize, pairs: Vec<(u32, f32)>) -> Document {
    Document {
        id: format!("d{id}"),
        vectors: BTreeMap::from([(
            FIELD.to_owned(),
            VectorField::Sparse(
                pairs
                    .into_iter()
                    .map(|(d, w)| (d, Impact::new(w)))
                    .collect(),
            ),
        )]),
        attrs: BTreeMap::new(),
    }
}

/// `rows` documents drawn from `vocab` dimensions, `nnz` non-zeros each, deterministically.
fn corpus(rows: usize, vocab: u32, nnz: usize) -> Vec<Document> {
    let mut rng: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    (0..rows)
        .map(|i| {
            let mut pairs: Vec<(u32, f32)> = Vec::with_capacity(nnz);
            let mut seen = std::collections::BTreeSet::new();
            while seen.len() < nnz {
                let d = (next() % u64::from(vocab)) as u32;
                if seen.insert(d) {
                    #[expect(clippy::cast_precision_loss, reason = "a test weight, not a metric")]
                    let w = (next() % 1000) as f32 / 1000.0 + 0.001;
                    pairs.push((d, w));
                }
            }
            pairs.sort_by_key(|(d, _)| *d);
            doc(i, pairs)
        })
        .collect()
}

#[test]
fn a_sparse_field_round_trips() {
    // ⚠️ Criterion 1. The dimensions must be exact -- a dimension is an identity, not a
    // measurement -- while the impact is quantized and may only be within the bound the
    // encoding states. `max_impact` is per TERM, so a global scale would decode every list
    // but the largest against the wrong one, and the error would look like a ranking bug.
    let docs = corpus(200, 500, 8);
    let p = sparse::build(&docs, FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).expect("the dictionary did not decode");

    let mut checked = 0usize;
    for (row, d) in docs.iter().enumerate() {
        let VectorField::Sparse(pairs) = &d.vectors[FIELD] else {
            panic!("not sparse")
        };
        for (dim, want) in pairs {
            let e = dict
                .lookup(*dim)
                .unwrap_or_else(|| panic!("dimension {dim} is not in the dictionary"));
            let raw = &p.section[e.offset as usize..e.offset as usize + e.bytes as usize];
            let list = dict.decode_list(&e, raw);
            let got = list
                .iter()
                .find(|(r, _)| *r as usize == row)
                .unwrap_or_else(|| panic!("row {row} is missing from list {dim}"));
            let bound = e.max_impact / 254.0;
            assert!(
                (got.1 - want.get()).abs() <= bound,
                "impact for row {row} dim {dim} decoded as {} against {} written, off by more \
                 than the stated bound {bound}",
                got.1,
                want.get()
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 200 * 8, "not every posting was checked");
}

#[test]
fn a_posting_list_holds_every_row_carrying_its_dimension_and_no_other() {
    // The transposition itself. A list that drops a row makes that document unfindable by
    // that term, which is a recall bug no round-trip of the rows it kept can see.
    let docs = corpus(300, 60, 5);
    let p = sparse::build(&docs, FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).unwrap();

    for dim in 0..60u32 {
        let mut want: Vec<u32> = Vec::new();
        for (row, d) in docs.iter().enumerate() {
            if let VectorField::Sparse(pairs) = &d.vectors[FIELD]
                && pairs.iter().any(|(x, _)| *x == dim)
            {
                want.push(row as u32);
            }
        }
        match dict.lookup(dim) {
            None => assert!(want.is_empty(), "dimension {dim} has rows but no entry"),
            Some(e) => {
                let raw = &p.section[e.offset as usize..e.offset as usize + e.bytes as usize];
                let got: Vec<u32> = dict
                    .decode_list(&e, raw)
                    .into_iter()
                    .map(|(r, _)| r)
                    .collect();
                assert_eq!(got, want, "list for dimension {dim} is wrong");
            }
        }
    }
}

#[test]
fn rows_in_a_list_are_ascending() {
    // ⚠️ Not decoration: the rows are stored as DELTAS, so an unsorted list encodes a
    // negative gap and decodes to garbage rows -- pointing at real documents, silently.
    let docs = corpus(400, 30, 6);
    let p = sparse::build(&docs, FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    for dim in 0..30u32 {
        let Some(e) = dict.lookup(dim) else { continue };
        let raw = &p.section[e.offset as usize..e.offset as usize + e.bytes as usize];
        let rows: Vec<u32> = dict
            .decode_list(&e, raw)
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert!(
            rows.windows(2).all(|w| w[0] < w[1]),
            "list {dim} is not strictly ascending: {rows:?}"
        );
    }
}

/// `rows` documents covering **every** one of `vocab` dimensions, `nnz` each.
///
/// ⚠️ Deliberately not the random generator: a random draw leaves some dimensions unused, so
/// two corpora of different sizes have different vocabularies and the scaling property cannot
/// be stated. Coverage has to be a property of the fixture, not a probability.
fn covering_corpus(rows: usize, vocab: u32, nnz: usize) -> Vec<Document> {
    assert!(
        rows * nnz >= vocab as usize,
        "fixture does not cover the vocabulary"
    );
    (0..rows)
        .map(|i| {
            let mut pairs: Vec<(u32, f32)> = (0..nnz)
                .map(|j| (((i * nnz + j) % vocab as usize) as u32, 0.25 + j as f32))
                .collect();
            pairs.sort_by_key(|(d, _)| *d);
            pairs.dedup_by_key(|(d, _)| *d);
            doc(i, pairs)
        })
        .collect()
}

#[test]
fn the_dictionary_scales_with_terms_not_documents() {
    // ⚠️ Criterion 4. A dictionary that grew per document would be a posting list per row --
    // the inverted index turned back the right way up -- and every query would fetch a
    // dictionary the size of the corpus.
    let small = sparse::build(&covering_corpus(100, 200, 4), FIELD, ImpactEncoding::U8);
    let large = sparse::build(&covering_corpus(1_000, 200, 4), FIELD, ImpactEncoding::U8);
    assert_eq!(Dictionary::decode(&small.dictionary).unwrap().len(), 200);
    assert_eq!(Dictionary::decode(&large.dictionary).unwrap().len(), 200);
    assert_eq!(
        small.dictionary.len(),
        large.dictionary.len(),
        "10x the rows over the same vocabulary changed the dictionary's size"
    );
    assert!(
        large.section.len() > small.section.len() * 5,
        "10x the rows did not grow the postings, so they are not being written"
    );
}

#[test]
fn the_dictionary_is_sorted_and_fixed_width_so_a_lookup_is_a_search() {
    // A dictionary is scanned only if it is not searchable. At 30,000 terms a scan per query
    // term is the difference between a lookup and a pass over 600 KB.
    let p = sparse::build(&corpus(50, 400, 10), FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    let dims: Vec<u32> = dict.dims().collect();
    assert!(
        dims.windows(2).all(|w| w[0] < w[1]),
        "the dictionary is not sorted by dimension"
    );
    assert!(
        dict.lookup(u32::MAX).is_none(),
        "an absent dimension was found"
    );
    for d in &dims {
        assert_eq!(dict.lookup(*d).map(|e| e.dim), Some(*d));
    }
}

#[test]
fn a_corrupt_dictionary_is_refused_not_guessed() {
    // A dictionary that decodes to nonsense addresses byte ranges that are not posting
    // lists, and every query returns confident garbage.
    assert!(Dictionary::decode(&[0xff, 0xfe]).is_none());
    let p = sparse::build(&corpus(10, 20, 3), FIELD, ImpactEncoding::U8);
    assert!(
        Dictionary::decode(&p.dictionary[..p.dictionary.len() - 3]).is_none(),
        "a truncated dictionary decoded"
    );
}

#[test]
fn a_document_with_no_sparse_field_contributes_nothing() {
    // Mixed corpora are the normal case: a dense-only document must not shift the row
    // numbering, because postings address SEGMENT rows and a skipped row shifts every later
    // one by exactly the amount nothing would notice.
    let mut docs = corpus(20, 40, 3);
    docs.insert(10, Document::new("dense-only", vec![1.0, 2.0]));
    let p = sparse::build(&docs, FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    for dim in dict.dims().collect::<Vec<_>>() {
        let e = dict.lookup(dim).unwrap();
        let raw = &p.section[e.offset as usize..e.offset as usize + e.bytes as usize];
        for (row, _) in dict.decode_list(&e, raw) {
            assert_ne!(row, 10, "the dense-only row appeared in a posting list");
            let VectorField::Sparse(pairs) = &docs[row as usize].vectors[FIELD] else {
                panic!("row {row} does not name the document that carries dimension {dim}")
            };
            assert!(pairs.iter().any(|(d, _)| *d == dim));
        }
    }
}

#[test]
fn f16_round_trips_within_its_precision() {
    // ⚠️ Hand-rolled bit arithmetic, so the two cases a naive conversion gets wrong are
    // asserted directly: a **subnormal** (below 2^-14, where the implicit leading 1 is gone)
    // and an **overflow** (above 65,504, which must saturate to infinity rather than wrap to
    // a small positive number — a wrapped impact would rank a document first).
    let weights: Vec<f32> = vec![
        0.0, 1.0, -1.0, 0.5, 0.1, -0.333, 1e-5, -1e-5, 6e-8, 65_504.0, 1e6, -1e6,
    ];
    let docs: Vec<Document> = weights
        .iter()
        .enumerate()
        .map(|(i, w)| doc(i, vec![(0, *w)]))
        .collect();
    let p = sparse::build(&docs, FIELD, ImpactEncoding::F16);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    let e = dict.lookup(0).unwrap();
    let got = dict.decode_list(&e, &p.section[e.offset as usize..][..e.bytes as usize]);
    assert_eq!(got.len(), weights.len());
    for ((_, out), want) in got.iter().zip(&weights) {
        if want.abs() > 65_504.0 {
            assert!(
                out.is_infinite() && out.signum() == want.signum(),
                "{want} became {out}"
            );
        } else if want.abs() < 6e-5 {
            // Subnormal territory: absolute error is what is bounded, not relative.
            assert!((out - want).abs() <= 6e-8, "subnormal {want} became {out}");
        } else {
            assert!(
                (out - want).abs() <= want.abs() / 1024.0,
                "{want} became {out}, outside binary16's 11-bit significand"
            );
        }
    }
}

#[test]
fn f32_impacts_are_exact() {
    // The oracle every quantized encoding is measured against. If this is lossy, criterion 11
    // measures two errors and attributes both to the cheap one.
    let weights = [0.1f32, -7.25, 1e-9, 3.402_823_5e38];
    let docs: Vec<Document> = weights
        .iter()
        .enumerate()
        .map(|(i, w)| doc(i, vec![(3, *w)]))
        .collect();
    let p = sparse::build(&docs, FIELD, ImpactEncoding::F32);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    let e = dict.lookup(3).unwrap();
    let got = dict.decode_list(&e, &p.section[e.offset as usize..][..e.bytes as usize]);
    assert_eq!(
        got.iter().map(|(_, w)| *w).collect::<Vec<_>>(),
        weights,
        "f32 impacts did not survive their own encoding"
    );
}

#[test]
fn an_encodings_width_is_what_it_writes() {
    // The width is used to slice the impact array; a width that disagrees with the writer
    // reads impacts from the middle of the neighbouring one.
    for (enc, w) in [
        (ImpactEncoding::U8, 1),
        (ImpactEncoding::F16, 2),
        (ImpactEncoding::F32, 4),
    ] {
        assert_eq!(enc.width(), w);
        let docs: Vec<Document> = (0..4).map(|i| doc(i, vec![(1, 0.5)])).collect();
        let p = sparse::build(&docs, FIELD, enc);
        let dict = Dictionary::decode(&p.dictionary).unwrap();
        let e = dict.lookup(1).unwrap();
        // 4 rows: 4 one-byte varints, then 4 impacts.
        assert_eq!(e.bytes as usize, 4 + 4 * w, "{enc:?} wrote the wrong width");
    }
}

// ---------------------------------------------------------------------------
// The segment side: a sparse field has to be addressable, and refusing to store
// one is better than storing nothing and saying yes.
// ---------------------------------------------------------------------------

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{DEFAULT_FIELD, Section, Segment, SegmentWriter};
use pstore_testkit::depth::DepthCounting;

#[test]
fn a_sparse_field_without_its_postings_is_refused() {
    // ⚠️ M3b's lesson, narrowed rather than deleted. The bug was never "sparse is
    // unsupported" -- it was a writer that ACCEPTED a document and stored nothing of it. The
    // section is supplied by whoever built the postings, so a writer that has not been given
    // one is in exactly that state again.
    let mut w = SegmentWriter::new(8);
    w.push(doc(0, vec![(1, 0.5)]));
    let err = w.try_finish().unwrap_err();
    assert!(
        format!("{err}").contains("SparsePostings"),
        "a sparse field was refused without naming what is missing: {err}"
    );

    let mut w = SegmentWriter::new(8);
    w.push(doc(0, vec![(1, 0.5)]));
    let p = sparse::build(&[doc(0, vec![(1, 0.5)])], FIELD, ImpactEncoding::U8);
    w.with_section(Section::SparsePostings, p.section)
        .try_finish()
        .expect("a sparse field with its postings attached was still refused");
}

#[tokio::test]
async fn a_sparse_field_has_a_layout_row() {
    // ⚠️ Without this the field is written and INVISIBLE: `seal_segment` skips any field
    // whose dims are 0, and a sparse field has no dense width, so the layout table would have
    // no row for it and `field_layout` would answer `None` for a field that is really there.
    let docs = corpus(20, 30, 4);
    let p = sparse::build(&docs, FIELD, ImpactEncoding::U8);
    let mut w = SegmentWriter::new(8);
    for d in &docs {
        w.push(d.clone());
    }
    let bytes = w
        .with_section(Section::SparsePostings, p.section.clone())
        .try_finish()
        .unwrap();

    let s = MemoryStore::new();
    let key = Key::new("t/idx/seg");
    s.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();

    let f = seg
        .field_layout(FIELD)
        .expect("the sparse field has no layout row");
    assert_eq!(f.kind, 1, "a sparse field is not tagged sparse");
    assert_eq!(
        f.vectors,
        Section::SparsePostings as u16,
        "the layout row does not point at the postings section"
    );
    let span = seg
        .field_section(FIELD, Section::SparsePostings)
        .expect("the postings section is not addressable through the field");
    assert_eq!(
        (span.end - span.start) as usize,
        p.section.len(),
        "the addressed span is not the postings"
    );
}

#[tokio::test]
async fn a_sparse_field_does_not_displace_the_dense_one() {
    // ⚠️ Field 0 keeps the LEGACY section ids, and fields are taken in NAME order. A sparse
    // field named `body_sparse` sorts before `vector`, so without a rule it would take slot 0
    // -- the dense field would move to the Field* ids, ids 2/3/4 would never be written, and
    // `VecIndex::search` would return zero rows with no error anywhere. Whether hybrid worked
    // would depend on how two field names happen to sort.
    let mut docs: Vec<Document> = Vec::new();
    for i in 0..12 {
        let mut d = Document::new(format!("d{i}"), vec![i as f32, 1.0]);
        d.vectors.insert(
            "aaa_sparse".to_owned(),
            VectorField::Sparse(vec![(1, Impact::new(0.5))]),
        );
        docs.push(d);
    }
    let p = sparse::build(&docs, "aaa_sparse", ImpactEncoding::U8);
    let mut w = SegmentWriter::new(4);
    for d in &docs {
        w.push(d.clone());
    }
    let bytes = w
        .with_section(Section::SparsePostings, p.section)
        .try_finish()
        .unwrap();

    let s = MemoryStore::new();
    let key = Key::new("t/idx/mixed");
    s.put(&key, bytes).await.unwrap();
    s.put(
        &pstore_format::sparse::dict_key(&key),
        bytes::Bytes::from(p.dictionary),
    )
    .await
    .unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();

    let dense = seg
        .field_layout(DEFAULT_FIELD)
        .expect("no dense layout row");
    assert_eq!(
        dense.vectors,
        Section::Vectors as u16,
        "the sparse field took the dense field's legacy section id"
    );
    assert!(
        seg.section(Section::Vectors).is_some(),
        "section id 2 was never written, so a reader predating the Fields table sees nothing"
    );
    // The dense vectors still read back, which is the observable half.
    let out = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(out.len(), 12);
    assert_eq!(out[3].field(DEFAULT_FIELD), [vec![3.0, 1.0]]);
}

#[test]
fn the_dictionary_key_is_derived_from_the_segments() {
    // No lookup, no catalog: a compaction has to find the sidecar of every input it merges,
    // and a LIST to discover it would be priced like a PUT and cap at 1000 keys.
    let a = Key::new("0f2a/tnt/7/idx/docs/seg/L0/0000-1.seg");
    assert_eq!(
        sparse::dict_key(&a),
        sparse::dict_key(&Key::new(a.as_str()))
    );
    assert_ne!(
        sparse::dict_key(&a),
        sparse::dict_key(&Key::new("other.seg"))
    );
    assert!(sparse::dict_key(&a).as_str().starts_with(a.as_str()));
    assert_ne!(sparse::dict_key(&a).as_str(), a.as_str());
}

#[tokio::test]
async fn a_scan_reconstructs_a_sparse_field() {
    // ⚠️ Compaction merges whatever `scan` returned. A scan that drops the sparse field does
    // not fail — it produces documents that look like documents that never had one, and the
    // merge makes that permanent.
    let docs = corpus(24, 40, 5);
    let (s, key) = put_sparse(&docs).await;
    let seg = Segment::open(&s, &key).await.unwrap();
    let out = seg.scan(&s, &key, None).await.unwrap();

    assert_eq!(out.len(), docs.len());
    for (got, want) in out.iter().zip(&docs) {
        let VectorField::Sparse(w) = &want.vectors[FIELD] else {
            panic!("fixture is not sparse")
        };
        let VectorField::Sparse(g) = out_field(got) else {
            panic!("{} came back with no sparse field", got.id)
        };
        assert_eq!(
            g.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
            w.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
            "{}'s dimensions changed",
            got.id
        );
        for ((dim, a), (_, b)) in g.iter().zip(w) {
            // ⚠️ The scale is the TERM's largest magnitude, not this posting's. A bound
            // written against the posting's own weight passes for the largest weight in
            // every list and fails for the smallest, which reads as a decoding bug.
            let bound = term_max(&docs, *dim) / 254.0;
            assert!(
                (a.get() - b.get()).abs() <= bound,
                "{}: dimension {dim} decoded as {} against {} written",
                got.id,
                a.get(),
                b.get()
            );
        }
    }
}

#[tokio::test]
async fn a_scan_of_a_sparse_segment_is_still_one_round() {
    // ⚠️ The postings span is known from the layout row and the sidecar key by derivation,
    // so neither waits on the other. Awaiting them in sequence gives the same documents at
    // three times the depth, and no functional test can tell.
    let docs = corpus(24, 40, 5);
    let (inner, key) = put_sparse(&docs).await;
    let s = DepthCounting::new(inner);
    let seg = Segment::open(&s, &key).await.unwrap();
    s.reset();
    let out = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(out.len(), docs.len());
    assert_eq!(
        s.depth(),
        1,
        "scanning a sparse segment took {} sequential rounds",
        s.depth()
    );
}

#[tokio::test]
async fn a_sparse_segment_without_its_dictionary_is_an_error_not_an_empty_field() {
    // ⚠️ The most dangerous confusion available here, and the same shape as the roster's:
    // "the sidecar is missing" must not read as "this document has no sparse field". The
    // second is a document a compaction would happily write out.
    let docs = corpus(8, 20, 3);
    let (s, key) = put_sparse(&docs).await;
    s.delete_batch(&[pstore_format::sparse::dict_key(&key)])
        .await
        .unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert!(
        seg.scan(&s, &key, None).await.is_err(),
        "a segment with unreachable postings scanned as though the field were absent"
    );
}

/// A segment carrying `docs`' sparse field, with its dictionary beside it.
async fn put_sparse(docs: &[Document]) -> (MemoryStore, Key) {
    let p = sparse::build(docs, FIELD, ImpactEncoding::U8);
    let mut w = SegmentWriter::new(8);
    for d in docs {
        w.push(d.clone());
    }
    let bytes = w
        .with_section(Section::SparsePostings, p.section)
        .try_finish()
        .unwrap();
    let s = MemoryStore::new();
    let key = Key::new("t/idx/sparse.seg");
    s.put(&key, bytes).await.unwrap();
    s.put(
        &pstore_format::sparse::dict_key(&key),
        bytes::Bytes::from(p.dictionary),
    )
    .await
    .unwrap();
    (s, key)
}

fn out_field(d: &Document) -> &VectorField {
    d.vectors
        .get(FIELD)
        .unwrap_or_else(|| panic!("{} has no field {FIELD}", d.id))
}

/// The largest magnitude any document gives `dim` — the scale a u8 impact is quantized by.
fn term_max(docs: &[Document], dim: u32) -> f32 {
    docs.iter()
        .filter_map(|d| match d.vectors.get(FIELD) {
            Some(VectorField::Sparse(p)) => Some(p),
            _ => None,
        })
        .flatten()
        .filter(|(x, _)| *x == dim)
        .fold(0.0f32, |m, (_, w)| m.max(w.get().abs()))
}

#[test]
fn a_dictionary_reports_the_encoding_it_was_built_with() {
    // ⚠️ The width the decoder slices impacts by. A dictionary that reported the wrong one
    // would read each impact from the middle of its neighbour — no error, no truncation,
    // just scores drawn from the wrong bytes.
    let docs = corpus(20, 30, 3);
    for enc in [ImpactEncoding::U8, ImpactEncoding::F16, ImpactEncoding::F32] {
        let p = sparse::build(&docs, FIELD, enc);
        assert_eq!(Dictionary::decode(&p.dictionary).unwrap().encoding(), enc);
    }
}

#[test]
fn a_dictionary_with_an_unknown_encoding_is_refused() {
    // Forward compatibility runs the other way for a SIDECAR than for a segment: a segment
    // skips sections it does not know, but a dictionary written by a newer encoder describes
    // every posting in the section. Guessing a width here would decode the whole field wrong.
    let mut p = sparse::build(&corpus(10, 20, 3), FIELD, ImpactEncoding::U8);
    p.dictionary[10] = 9;
    assert!(Dictionary::decode(&p.dictionary).is_none());

    let mut wrong_version = sparse::build(&corpus(10, 20, 3), FIELD, ImpactEncoding::U8);
    wrong_version.dictionary[8] = 7;
    assert!(
        Dictionary::decode(&wrong_version.dictionary).is_none(),
        "a dictionary from a future version decoded"
    );
}

#[test]
fn a_field_with_no_postings_has_an_empty_dictionary() {
    // ⚠️ Distinguishable from a field that has postings. A dictionary that reported entries
    // it does not have addresses byte ranges outside the section.
    let docs: Vec<Document> = (0..5)
        .map(|i| Document::new(format!("d{i}"), vec![1.0]))
        .collect();
    let p = sparse::build(&docs, FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    assert!(dict.is_empty());
    assert_eq!(dict.len(), 0);
    assert!(p.section.is_empty());
    assert!(dict.lookup(0).is_none());
}

#[test]
fn a_truncated_posting_list_decodes_to_nothing_rather_than_to_rows() {
    // ⚠️ Rows are deltas, so a list cut short does not decode to *fewer* rows — it decodes to
    // rows built from whatever bytes follow, pointing at real documents. Refusing is the only
    // safe answer, and returning a short list would look like a correct answer.
    let p = sparse::build(&corpus(40, 10, 4), FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    let e = dict.lookup(dict.dims().next().unwrap()).unwrap();
    let full = &p.section[e.offset as usize..][..e.bytes as usize];
    assert!(!dict.decode_list(&e, full).is_empty());

    // Cut inside the impacts: the rows decode, the impacts do not.
    let cut = &full[..full.len() - 1];
    assert!(dict.decode_list(&e, cut).len() < e.count as usize);
    // Cut inside the row deltas: nothing at all.
    assert!(dict.decode_list(&e, &full[..1]).is_empty());
    assert!(dict.decode_list(&e, &[]).is_empty());
    // A varint that never terminates must not loop or wrap.
    assert!(dict.decode_list(&e, &[0xff; 32]).is_empty());
}

#[test]
fn transpose_refuses_a_section_that_does_not_hold_its_own_postings() {
    // A truncated section reaching `scan` would otherwise reconstruct documents from the
    // bytes that happened to be there, and a compaction would write them out.
    let docs = corpus(30, 25, 4);
    let p = sparse::build(&docs, FIELD, ImpactEncoding::U8);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    let full = sparse::transpose(&dict, &p.section, docs.len());
    assert!(full.iter().all(|r| !r.is_empty()));
    let short = sparse::transpose(&dict, &p.section[..p.section.len() / 3], docs.len());
    assert!(
        short.iter().map(Vec::len).sum::<usize>() < full.iter().map(Vec::len).sum::<usize>(),
        "a truncated section reconstructed every posting"
    );
    // A row count smaller than the postings address must drop, never panic or wrap.
    assert_eq!(sparse::transpose(&dict, &p.section, 0).len(), 0);
}

#[test]
fn f16_saturates_rather_than_wrapping() {
    // ⚠️ Both directions of overflow, because both are silent. A value above binary16's range
    // must become infinity, not wrap to a small positive number that ranks first; a NaN must
    // stay a NaN rather than becoming the largest finite impact.
    let weights: Vec<f32> = vec![
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        // Just below the maximum, and rounds UP past it: the carry-into-the-exponent case.
        65_520.0,
        -65_520.0,
    ];
    let docs: Vec<Document> = weights
        .iter()
        .enumerate()
        .map(|(i, w)| doc(i, vec![(0, *w)]))
        .collect();
    let p = sparse::build(&docs, FIELD, ImpactEncoding::F16);
    let dict = Dictionary::decode(&p.dictionary).unwrap();
    let e = dict.lookup(0).unwrap();
    let got = dict.decode_list(&e, &p.section[e.offset as usize..][..e.bytes as usize]);
    assert!(got[0].1.is_infinite() && got[0].1.is_sign_positive());
    assert!(got[1].1.is_infinite() && got[1].1.is_sign_negative());
    assert!(got[2].1.is_nan(), "a NaN became {}", got[2].1);
    assert!(
        got[3].1.is_infinite() && got[3].1.is_sign_positive(),
        "65520 rounded to {} instead of saturating",
        got[3].1
    );
    assert!(got[4].1.is_infinite() && got[4].1.is_sign_negative());
}
