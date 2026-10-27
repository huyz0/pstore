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
        text::analyze(&text::Analyzer::default(), "the Quick, quick brown dog!"),
        ["the", "quick", "quick", "brown", "dog"]
    );
    assert_eq!(
        text::analyze(&text::Analyzer::default(), "FOX and dog"),
        ["fox", "and", "dog"]
    );
    assert_eq!(
        text::analyze(&text::Analyzer::default(), "  ---  "),
        Vec::<String>::new()
    );
    assert_eq!(
        text::analyze(&text::Analyzer::default(), "a1 b_2"),
        ["a1", "b", "2"]
    );
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
        let tokens = text::analyze(&text::Analyzer::default(), body);
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
    // MAGIC(8) VERSION(2) terms(4) blob_len(4) rows(4) total_tokens(8) = 30, then 22-byte
    // entries whose first six bytes are the term pointer.
    const HEADER: usize = 30;
    const ENTRY: usize = 22;
    let (a, b) = (HEADER, HEADER + ENTRY);
    let first: Vec<u8> = unsorted[a..a + 6].to_vec();
    let second: Vec<u8> = unsorted[b..b + 6].to_vec();
    unsorted[a..a + 6].copy_from_slice(&second);
    unsorted[b..b + 6].copy_from_slice(&first);
    assert!(
        TermDict::decode(&unsorted).is_none(),
        "an unsorted term dictionary decoded, and every lookup past the swap would miss"
    );

    // ⚠️ A DUPLICATE term, which is neither unsorted nor short. `binary_search` returns an
    // arbitrary one of a run of equals, so one entry's postings become unreachable — a term
    // that is in the index and cannot be found.
    let mut dupes = text::build(&corpus(), text::DEFAULT_TEXT_FIELD).dictionary;
    let first: Vec<u8> = dupes[a..a + 6].to_vec();
    dupes[b..b + 6].copy_from_slice(&first);
    assert!(
        TermDict::decode(&dupes).is_none(),
        "a dictionary with a duplicate term decoded, and one of the two lists is unreachable"
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

/// M63: `tokens` is `analyze` with the byte span of each token it came from.
#[test]
fn tokens_spans_cover_what_analyze_returns() {
    use pstore_format::text::{Analyzer, Language, analyze, tokens};
    let english = Analyzer {
        language: Language::English,
        stemming: true,
        remove_stopwords: true,
        case_sensitive: false,
        ascii_folding: true,
        ..Analyzer::default()
    };
    let sensitive = Analyzer {
        case_sensitive: true,
        remove_stopwords: true,
        ..Analyzer::default()
    };
    let analyzers = [Analyzer::default(), english, sensitive];
    let texts = [
        "The quick brown foxes were running, and Runs!",
        "İstanbul ½ café cafe\u{301} CAFÉ",
        "The THE the",
        "  leading and trailing  ",
        "中文测试 mixed 中文 text",
        "",
        "...---...",
        "a1 b2-c3_d4 ünï",
    ];
    for a in &analyzers {
        for s in texts {
            let toks = tokens(a, s);
            // The terms are `analyze`'s, in order: a stopword has a span and no term.
            let terms: Vec<String> = toks.iter().filter_map(|t| t.term.clone()).collect();
            assert_eq!(terms, analyze(a, s), "{a:?} {s:?}");
            let mut end = 0;
            for t in &toks {
                assert!(
                    t.span.start >= end,
                    "overlap or disorder in {s:?}: {toks:?}"
                );
                end = t.span.end;
                let piece = &s[t.span.clone()];
                // One maximal alphanumeric run of the input.
                assert!(
                    !piece.is_empty() && piece.chars().all(char::is_alphanumeric),
                    "{piece:?}"
                );
                assert!(
                    s[..t.span.start]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !c.is_alphanumeric()),
                    "{piece:?} starts mid-run in {s:?}"
                );
                assert!(
                    s[t.span.end..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_alphanumeric()),
                    "{piece:?} ends mid-run in {s:?}"
                );
                // Analyzed alone, the token gives its own term, or none.
                assert_eq!(
                    analyze(a, piece).into_iter().next(),
                    t.term,
                    "{a:?} {piece:?} in {s:?}"
                );
            }
        }
    }
    // A stopword has a span and no term; case decides under a case-sensitive analyzer.
    let t = tokens(&sensitive, "The the");
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].term.as_deref(), Some("The"));
    assert_eq!(t[1].term, None);
}

#[test]
fn extend_over_marks_keeps_an_accent_with_its_letter() {
    use pstore_format::text::extend_over_marks;
    // `cafe` + U+0301 (two bytes) + `x`: the token `cafe` ends at 4, its accent at 6.
    let s = "cafe\u{301}x";
    assert_eq!(extend_over_marks(s, 4), 6);
    // No mark after the end: unchanged. A letter is not a mark.
    assert_eq!(extend_over_marks(s, 6), 6);
    assert_eq!(extend_over_marks("cafex", 4), 4);
    // Several marks in a row, and the end of the text.
    let two = "a\u{301}\u{323}";
    assert_eq!(extend_over_marks(two, 1), two.len());
    assert_eq!(extend_over_marks(two, two.len()), two.len());
    // An end past the text, or inside a character, extends nothing and never panics.
    assert_eq!(extend_over_marks("ab", 9), 9);
    assert_eq!(extend_over_marks("\u{301}", 1), 1);
}

#[test]
fn extending_over_marks_stops_at_the_next_token() {
    // Code review: U+0345 is a combining mark that is also alphabetic, so it starts a token.
    // Extending a marked token over it would overlap that token, and mark it twice.
    use pstore_format::text::{Analyzer, extend_over_marks, tokens};
    assert!(
        '\u{345}'.is_alphanumeric(),
        "the premise: it is a letter to the splitter"
    );
    let s = "e\u{301}\u{345}x";
    let toks = tokens(&Analyzer::default(), s);
    assert_eq!(toks.len(), 2, "{toks:?}");
    let end = extend_over_marks(s, toks[0].span.end);
    assert!(
        end <= toks[1].span.start,
        "{end} overlaps {:?}",
        toks[1].span
    );
    assert_eq!(&s[toks[0].span.start..end], "e\u{301}");
}

#[test]
fn a_dictionary_is_decoded_once_a_buffer() {
    // M122: a text query decoded every text segment's dictionary again, though the pinned
    // cache hands back the same buffer each time. Kept by the buffer: the same `Bytes` is one
    // decoded dictionary; the same bytes in another buffer are another, which answers the
    // same; a refusal is refused again.
    let built = text::build(&corpus(), text::DEFAULT_TEXT_FIELD);
    let raw = bytes::Bytes::from(built.dictionary.clone());
    let one = TermDict::decode_shared(&raw).unwrap();
    let two = TermDict::decode_shared(&raw.clone()).unwrap();
    assert!(std::sync::Arc::ptr_eq(&one, &two));
    let other = bytes::Bytes::from(built.dictionary.clone());
    let three = TermDict::decode_shared(&other).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&one, &three));
    let fresh = TermDict::decode(&built.dictionary).unwrap();
    for d in [&one, &three] {
        assert_eq!(
            d.terms().collect::<Vec<_>>(),
            fresh.terms().collect::<Vec<_>>()
        );
        for t in fresh.terms() {
            assert_eq!(d.lookup(t), fresh.lookup(t), "{t}");
        }
    }
    let cut = raw.slice(..raw.len() - 1);
    assert!(TermDict::decode_shared(&cut).is_none());
    assert!(TermDict::decode_shared(&cut).is_none());
}

#[test]
fn a_string_array_indexes_as_its_strings_joined_by_a_space() {
    // M150 criterion 1: every analyzer, and rows whose arrays mix strings with other values.
    let analyzers = [
        text::Analyzer::default(),
        text::Analyzer {
            stemming: true,
            remove_stopwords: true,
            ..text::Analyzer::default()
        },
        text::Analyzer {
            case_sensitive: true,
            ..text::Analyzer::default()
        },
        text::Analyzer {
            ascii_folding: true,
            ..text::Analyzer::default()
        },
    ];
    let pool = ["the Quick fox", "", "Café-au-lait", "running dogs", "fox"];
    let s = |x: &str| Value::Str(x.to_owned());
    let mut values: Vec<Option<Value>> = vec![
        None,
        Some(Value::Int(5)),
        Some(s("")),
        Some(s("quick brown FOX")),
        Some(Value::Array(vec![])),
        Some(Value::Array(vec![s("")])),
        Some(Value::Array(vec![Value::Int(5)])),
        Some(Value::Array(vec![s("a b"), Value::Int(5), s("c")])),
    ];
    for a in pool {
        values.push(Some(Value::Array(vec![s(a)])));
        for b in pool {
            values.push(Some(Value::Array(vec![s(a), s(b)])));
            values.push(Some(Value::Array(vec![s(b), s(a), s(a)])));
        }
    }
    let rows = |joined: bool| -> Vec<Document> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.0]);
                let v = match (v, joined) {
                    (Some(Value::Array(items)), true) => Some(Value::Str(
                        items
                            .iter()
                            .filter_map(|e| match e {
                                Value::Str(x) => Some(x.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    )),
                    (v, _) => v.clone(),
                };
                if let Some(v) = v {
                    d.attrs.insert(text::DEFAULT_TEXT_FIELD.to_owned(), v);
                }
                d
            })
            .collect()
    };
    for a in &analyzers {
        let arrays = text::build_with(&rows(false), text::DEFAULT_TEXT_FIELD, a);
        let joined = text::build_with(&rows(true), text::DEFAULT_TEXT_FIELD, a);
        assert_eq!(arrays.fieldnorms, joined.fieldnorms, "{a:?}");
        assert_eq!(arrays.postings, joined.postings, "{a:?}");
        assert_eq!(arrays.dictionary, joined.dictionary, "{a:?}");
        assert!(joined.fieldnorms.iter().filter(|n| **n > 0).count() > values.len() / 2);
    }
}
