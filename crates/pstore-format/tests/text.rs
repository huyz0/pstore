//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The full-text layout: an analyzer, a term dictionary carrying document frequency, and
//! postings whose impact is an **exact** term frequency.
//!
//! ⚠️ The text itself stays in the document's attributes. Postings cannot be inverted back
//! into text — order, duplicates and dropped tokens are gone — so a compaction that merged
//! only what the postings hold would rewrite every document as a bag of words. Keeping the
//! string is what makes re-analysis at merge time possible at all.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::text::{self, TermDict};
use pstore_format::{Document, Section, Segment, SegmentWriter, Value};

fn doc(id: usize, body: &str) -> Document {
    let mut d = Document::new(format!("d{id:05}"), vec![1.0, 0.0]);
    d.attrs.insert(
        text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str(body.to_owned()),
    );
    d
}

const CORPUS: [&str; 6] = [
    "the quick brown fox",
    "the Quick, quick brown dog!",
    "lazy dog sleeps",
    "FOX and dog and fox and fox",
    "",
    "the the the the",
];

fn corpus() -> Vec<Document> {
    CORPUS.iter().enumerate().map(|(i, s)| doc(i, s)).collect()
}

#[test]
fn the_analyzer_lowercases_and_splits_on_non_alphanumeric() {
    // ⚠️ The whole analysis chain, and it is deliberately this small: stemming, stopwords and
    // language rules are quality knobs, and a knob chosen without an eval set is noise. What
    // it MUST do is be a function of the text alone, because changing it changes every
    // posting and therefore requires reindexing.
    assert_eq!(
        text::analyze("the Quick, quick brown dog!"),
        ["the", "quick", "quick", "brown", "dog"]
    );
    assert_eq!(text::analyze("FOX and dog"), ["fox", "and", "dog"]);
    assert_eq!(text::analyze("  ---  "), Vec::<String>::new());
    assert_eq!(text::analyze("a1 b_2"), ["a1", "b", "2"]);
}

#[test]
fn a_text_field_round_trips() {
    // ⚠️ Term frequency is an INTEGER and must survive exactly: the impact encoding u8 scales
    // per term, so a tf of 3 in a list whose maximum is 4 comes back as 2.99 -- which BM25
    // then saturates differently, and the oracle disagrees for a reason no test names.
    let docs = corpus();
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let dict = TermDict::decode(&built.dictionary).expect("the dictionary did not decode");

    for (row, d) in docs.iter().enumerate() {
        let Some(Value::Str(body)) = d.attrs.get(text::DEFAULT_TEXT_FIELD) else {
            panic!("fixture has no text")
        };
        let tokens = text::analyze(body);
        assert_eq!(
            built.fieldnorms[row],
            tokens.len() as u32,
            "row {row}'s fieldnorm is not its token count"
        );
        let mut want: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for t in &tokens {
            *want.entry(t.as_str()).or_insert(0) += 1;
        }
        for (term, tf) in want {
            let e = dict
                .lookup(term)
                .unwrap_or_else(|| panic!("term {term:?} is not in the dictionary"));
            let list =
                dict.decode_list(&e, &built.postings[e.offset as usize..][..e.bytes as usize]);
            let got = list
                .iter()
                .find(|(r, _)| *r as usize == row)
                .unwrap_or_else(|| panic!("row {row} missing from {term:?}"));
            assert_eq!(got.1, tf, "term {term:?} in row {row}");
        }
    }
}

#[test]
fn the_dictionary_carries_document_frequency() {
    // ⚠️ The number two-pass IDF is made of. A `df` that counted POSTINGS rather than
    // DOCUMENTS is right for every term that appears at most once per document -- which is
    // most of them -- and wrong for exactly the common terms IDF is meant to discount.
    let docs = corpus();
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let dict = TermDict::decode(&built.dictionary).unwrap();

    // "the" appears 4 times in row 5 and once each in rows 0 and 1: df = 3, not 6.
    let e = dict.lookup("the").unwrap();
    assert_eq!(e.df, 3, "df counted postings rather than documents");
    assert_eq!(e.count, 3, "the posting list has one entry per document");
    // "fox": rows 0 and 3.
    assert_eq!(dict.lookup("fox").unwrap().df, 2);
    // "dog": rows 1, 2, 3.
    assert_eq!(dict.lookup("dog").unwrap().df, 3);
    assert!(dict.lookup("aardvark").is_none());
}

#[test]
fn a_document_without_text_does_not_shift_the_rows() {
    // ⚠️ Postings address SEGMENT rows. A document with no text skipped rather than counted
    // moves every later row by one, and every hit afterwards names the wrong document -- with
    // scores that are internally consistent and a top-k that looks fine.
    let mut docs = corpus();
    docs.insert(2, Document::new("no-text", vec![1.0, 0.0]));
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let dict = TermDict::decode(&built.dictionary).unwrap();

    assert_eq!(built.fieldnorms.len(), docs.len());
    assert_eq!(
        built.fieldnorms[2], 0,
        "a document with no text has no length"
    );
    let e = dict.lookup("lazy").unwrap();
    let list = dict.decode_list(&e, &built.postings[e.offset as usize..][..e.bytes as usize]);
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].0, 3,
        "\"lazy\" is in the row after the untexted one, not before it"
    );
}

#[test]
fn a_dictionary_of_terms_is_searchable_and_refuses_nonsense() {
    let built = text::build(&corpus(), text::DEFAULT_TEXT_FIELD);
    let dict = TermDict::decode(&built.dictionary).unwrap();
    let terms: Vec<String> = dict.terms().map(str::to_owned).collect();
    assert!(
        terms.windows(2).all(|w| w[0] < w[1]),
        "the term dictionary is not sorted: {terms:?}"
    );
    for t in &terms {
        assert_eq!(dict.lookup(t).map(|e| e.df > 0), Some(true));
    }
    // A term between two entries, which is where a search's terminating condition shows.
    assert!(dict.lookup("dogz").is_none());
    assert!(dict.lookup("").is_none());
    assert!(TermDict::decode(&[0xff, 0xfe]).is_none());
    assert!(TermDict::decode(&built.dictionary[..built.dictionary.len() - 3]).is_none());
}

#[test]
fn the_term_dictionary_scales_with_terms_not_documents() {
    // A dictionary that grew per document is the inverted index turned back the right way up,
    // and every query would fetch a dictionary the size of the corpus.
    let vocab = ["alpha", "beta", "gamma", "delta"];
    let small: Vec<Document> = (0..20)
        .map(|i| doc(i, &format!("{} {}", vocab[i % 4], vocab[(i + 1) % 4])))
        .collect();
    let large: Vec<Document> = (0..2_000)
        .map(|i| doc(i, &format!("{} {}", vocab[i % 4], vocab[(i + 1) % 4])))
        .collect();
    let a = text::build(&small, text::DEFAULT_TEXT_FIELD);
    let b = text::build(&large, text::DEFAULT_TEXT_FIELD);
    assert_eq!(TermDict::decode(&a.dictionary).unwrap().len(), 4);
    assert_eq!(TermDict::decode(&b.dictionary).unwrap().len(), 4);
    assert_eq!(
        a.dictionary.len(),
        b.dictionary.len(),
        "100x the documents changed the dictionary's size"
    );
    assert!(b.postings.len() > a.postings.len() * 50);
}

#[tokio::test]
async fn a_text_segment_carries_its_postings_and_norms_but_no_field_row() {
    // ⚠️ **No `Fields` row.** That table describes VECTOR fields, whose layout a reader must
    // know to decode them; a text field's presence is whether its section exists. A row would
    // send the postings to `decode_field`, which reads them as f32 and returns a dense field
    // of noise -- the failure M5a's criterion 4 exists for, reintroduced by a table entry
    // nothing needed.
    let docs = corpus();
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let mut w = SegmentWriter::new(8);
    for d in &docs {
        w.push(d.clone());
    }
    let bytes = w
        .with_section(Section::TextPostings, built.postings.clone())
        .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
        .try_finish()
        .unwrap();

    let s = MemoryStore::new();
    let key = Key::new("t/idx/text.seg");
    s.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();

    assert!(
        seg.has_text(),
        "a segment with postings does not report text"
    );
    assert!(seg.section(Section::TextPostings).is_some());
    assert!(seg.section(Section::Fieldnorms).is_some());
    assert!(
        seg.field_layout(text::DEFAULT_TEXT_FIELD).is_none(),
        "a text field took a vector layout row"
    );
    // The strings themselves come back from the attributes, which is what a compaction
    // re-analyzes.
    let out = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(out.len(), docs.len());
    assert_eq!(
        out[3].attrs.get(text::DEFAULT_TEXT_FIELD),
        docs[3].attrs.get(text::DEFAULT_TEXT_FIELD),
        "the text a merge would re-analyze did not survive the scan"
    );
    // A dense-only segment says so.
    let mut w2 = SegmentWriter::new(8);
    w2.push(Document::new("plain", vec![1.0, 0.0]));
    let plain = Key::new("t/idx/plain.seg");
    s.put(&plain, w2.try_finish().unwrap()).await.unwrap();
    assert!(!Segment::open(&s, &plain).await.unwrap().has_text());
}

#[test]
fn a_dictionary_from_another_format_or_an_unsorted_one_is_refused() {
    // ⚠️ Two failures with no symptom. A sidecar from a different writer decodes term offsets
    // into a blob that is not a term blob; an unsorted table cannot be binary-searched, and
    // searching it anyway answers "absent" for terms that are present — a silent recall loss
    // rather than an error.
    let mut wrong_magic = text::build(&corpus(), text::DEFAULT_TEXT_FIELD).dictionary;
    wrong_magic[..8].copy_from_slice(b"NOTATERM");
    assert!(TermDict::decode(&wrong_magic).is_none());

    let mut wrong_version = text::build(&corpus(), text::DEFAULT_TEXT_FIELD).dictionary;
    wrong_version[8] = 9;
    assert!(TermDict::decode(&wrong_version).is_none());

    // Swap the first two entries' term pointers, which unsorts the table without changing
    // its length.
    let mut unsorted = text::build(&corpus(), text::DEFAULT_TEXT_FIELD).dictionary;
    let (a, b) = (18 + 8, 18 + 8 + 22);
    let first: Vec<u8> = unsorted[a..a + 6].to_vec();
    let second: Vec<u8> = unsorted[b..b + 6].to_vec();
    unsorted[a..a + 6].copy_from_slice(&second);
    unsorted[b..b + 6].copy_from_slice(&first);
    assert!(
        TermDict::decode(&unsorted).is_none(),
        "an unsorted term dictionary decoded, and every lookup past the swap would miss"
    );
}

#[test]
fn a_corpus_with_no_text_has_an_empty_dictionary() {
    // Distinguishable from a corpus that has terms: a dictionary reporting entries it does
    // not have addresses byte ranges outside the section.
    let docs: Vec<Document> = (0..4)
        .map(|i| Document::new(format!("d{i}"), vec![1.0, 0.0]))
        .collect();
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let dict = TermDict::decode(&built.dictionary).unwrap();
    assert!(dict.is_empty());
    assert_eq!(dict.len(), 0);
    assert_eq!(dict.doc_count(), 4);
    assert_eq!(dict.total_tokens(), 0);
    assert!(built.postings.is_empty());
    assert!(dict.lookup("anything").is_none());
}
