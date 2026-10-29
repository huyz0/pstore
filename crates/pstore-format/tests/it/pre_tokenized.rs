//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M158 criterion 1: under `pre_tokenized_array` a document's string is one token, kept byte
//! for byte, and a query operand is its tokens joined by U+001F.

use pstore_format::text::{
    Analyzer, FullText, Tokenizer, analyze, analyze_query, query_tokens, tokens,
};

fn pre() -> Analyzer {
    Analyzer {
        tokenizer: Tokenizer::PreTokenizedArray,
        case_sensitive: true,
        ..Analyzer::default()
    }
}

#[test]
fn a_document_string_is_one_token_kept_whole() {
    let a = pre();
    assert_eq!(analyze(&a, "Running Late"), ["Running Late"]);
    assert_eq!(analyze(&a, "a\u{1F}b"), ["a\u{1F}b"]);
    assert!(analyze(&a, "").is_empty());
    let t = tokens(&a, "Running Late");
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].span, 0.."Running Late".len());
}

#[test]
fn a_query_operand_is_its_pieces() {
    let a = pre();
    let q = "a\u{1F}B c\u{1F}\u{1F}d";
    assert_eq!(analyze_query(&a, q), ["a", "B c", "d"]);
    let spans: Vec<_> = query_tokens(&a, q).into_iter().map(|t| t.span).collect();
    assert_eq!(spans, [0..1, 2..5, 7..8]);
    // `word_v1` reads a query as it reads a document.
    let w = Analyzer::default();
    for s in ["Running Late", "a\u{1F}b", "", q] {
        assert_eq!(analyze_query(&w, s), analyze(&w, s), "{s:?}");
        assert_eq!(query_tokens(&w, s), tokens(&w, s), "{s:?}");
    }
}

#[test]
fn the_tokenizer_round_trips_and_word_v1_is_unchanged() {
    let p = FullText {
        analyzer: pre(),
        k1: 1.5,
        ..FullText::default()
    };
    let line = p.encode();
    assert!(line.starts_with("v2;"), "{line}");
    assert_eq!(FullText::decode(&line), Some(p));
    // Today's line, byte for byte.
    assert_eq!(
        FullText::default().encode(),
        "v1;english;0000;3f99999a;3f400000"
    );
    assert_eq!(
        FullText::decode(&FullText::default().encode()),
        Some(FullText::default())
    );
}
