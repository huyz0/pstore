//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M181: the `word_v0` to `word_v4` tokenizers, `max_token_length`, and their encoding.

use pstore_format::text::{
    self, Analyzer, FullText, Language, Tokenizer, analyze, analyze_query, tokens,
};
use pstore_format::{Document, Value};

const WORD: [Tokenizer; 5] = [
    Tokenizer::WordV0,
    Tokenizer::WordV1,
    Tokenizer::WordV2,
    Tokenizer::WordV3,
    Tokenizer::WordV4,
];

fn with(tokenizer: Tokenizer) -> Analyzer {
    Analyzer {
        tokenizer,
        ..Analyzer::default()
    }
}

fn limited(tokenizer: Tokenizer, max: u8) -> Analyzer {
    Analyzer {
        tokenizer,
        max_token_length: Some(max),
        ..Analyzer::default()
    }
}

/// The raw tokens of `s`, as the strings their spans cut.
fn raw(a: &Analyzer, s: &str) -> Vec<String> {
    tokens(a, s)
        .into_iter()
        .map(|t| s[t.span].to_owned())
        .collect()
}

/// The table of AC1, written from the definitions by hand: (input, `word_v0`/`word_v1`,
/// `word_v2`, `word_v3`/`word_v4`).
type Row = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
);

const GOLDEN: &[Row] = &[
    (
        "can't stop 3.14 e-mail",
        &["can", "t", "stop", "3", "14", "e", "mail"],
        &["can", "t", "stop", "3", "14", "e", "mail"],
        &["can't", "stop", "3.14", "e", "mail"],
    ),
    (
        "cafe\u{301} x",
        &["cafe", "x"],
        &["cafe", "x"],
        &["cafe\u{301}", "x"],
    ),
    (
        "東京タワー",
        &["東京タワー"],
        &["東", "京", "タワー"],
        &["東", "京", "タワー"],
    ),
    (
        "rust是好的",
        &["rust是好的"],
        &["rust", "是", "好", "的"],
        &["rust", "是", "好", "的"],
    ),
    (
        "\u{1F469}\u{200D}\u{1F4BB} dev",
        &["dev"],
        &["\u{1F469}\u{200D}\u{1F4BB}", "dev"],
        &["\u{1F469}\u{200D}\u{1F4BB}", "dev"],
    ),
    ("🇯🇵", &[], &["🇯🇵"], &["🇯🇵"]),
    ("1\u{FE0F}\u{20E3}", &["1"], &["1"], &["1\u{FE0F}\u{20E3}"]),
    ("", &[], &[], &[]),
    (" \t\n ", &[], &[], &[]),
];

#[test]
fn word_tokenizers_split_as_specified() {
    for &(input, v0, v2, v3) in GOLDEN {
        for (t, want) in [
            (Tokenizer::WordV0, v0),
            (Tokenizer::WordV1, v0),
            (Tokenizer::WordV2, v2),
            (Tokenizer::WordV3, v3),
            (Tokenizer::WordV4, v3),
        ] {
            let a = with(t);
            assert_eq!(raw(&a, input), want, "{t:?} {input:?}");
            // The default analyzer: each raw token's term is the token lowercased.
            let lower: Vec<String> = want.iter().map(|w| w.to_lowercase()).collect();
            assert_eq!(analyze(&a, input), lower, "{t:?} {input:?}");
        }
    }
}

/// Beyond the table: ideographs before an emoji are split as after one.
#[test]
fn ideographs_before_an_emoji_are_split() {
    assert_eq!(
        raw(&with(Tokenizer::WordV2), "東京👍x"),
        ["東", "京", "👍", "x"]
    );
    assert_eq!(raw(&with(Tokenizer::WordV1), "東京👍x"), ["東京", "x"]);
}

#[test]
fn every_tokenizer_runs_one_pipeline() {
    let inputs = [
        "The RUNNING Cafés",
        "the running cafe\u{301}s",
        "Running 東京 dogs 🇯🇵",
        "",
    ];
    for t in WORD {
        let a = Analyzer {
            language: Language::English,
            stemming: true,
            remove_stopwords: true,
            ascii_folding: true,
            ..with(t)
        };
        // Case, folding, the stopword and the stem, per raw token.
        assert_eq!(analyze(&a, "The RUNNING Cafés"), ["run", "cafe"], "{t:?}");
        let sensitive = Analyzer {
            case_sensitive: true,
            stemming: false,
            ..a
        };
        assert_eq!(analyze(&sensitive, "The Dogs"), ["The", "Dogs"], "{t:?}");
        for s in inputs {
            let terms: Vec<String> = tokens(&a, s).into_iter().filter_map(|x| x.term).collect();
            assert_eq!(analyze(&a, s), terms, "{t:?} {s:?}");
            assert_eq!(analyze_query(&a, s), analyze(&a, s), "{t:?} {s:?}");
        }
    }
}

#[test]
fn a_token_over_the_limit_has_no_term() {
    for t in WORD {
        let a = limited(t, 5);
        assert_eq!(analyze(&a, "abcde abcdef"), ["abcde"], "{t:?}");
        assert_eq!(tokens(&a, "abcde abcdef").len(), 2, "{t:?}");
        // Characters, not bytes: ten bytes, five characters.
        assert_eq!(
            analyze(&a, "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"),
            ["ééééé"],
            "{t:?}"
        );
        // Counted before folding: `e` U+0301 is two characters, folded to one.
        let folded = Analyzer {
            ascii_folding: true,
            ..limited(t, 1)
        };
        assert!(text::over_limit(&folded, "e\u{301}"), "{t:?}");
    }
    // Characters, not graphemes: one grapheme of three characters.
    let emoji = "\u{1F469}\u{200D}\u{1F4BB}";
    assert!(analyze(&limited(Tokenizer::WordV3, 2), emoji).is_empty());
    assert_eq!(analyze(&limited(Tokenizer::WordV3, 3), emoji), [emoji]);
    assert!(!text::over_limit(
        &with(Tokenizer::WordV3),
        &"a".repeat(1000)
    ));
}

#[test]
fn a_dropped_token_is_not_in_the_fieldnorm() {
    let mut d = Document::new("a", vec![1.0, 0.0]);
    d.attrs.insert(
        text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str("abcde abcdef".to_owned()),
    );
    let a = limited(Tokenizer::WordV1, 5);
    let built = text::build_with(&[d], text::DEFAULT_TEXT_FIELD, &a);
    assert_eq!(built.fieldnorms, [1]);
    let dict = text::TermDict::decode(&built.dictionary).unwrap();
    assert!(dict.lookup("abcde").is_some());
    assert!(dict.lookup("abcdef").is_none());
    assert_eq!(dict.total_tokens(), 1);
}

fn full(analyzer: Analyzer) -> FullText {
    FullText {
        analyzer,
        k1: 1.5,
        ..FullText::default()
    }
}

#[test]
fn every_tokenizer_and_limit_round_trips() {
    for t in Tokenizer::ALL {
        for max in [None, Some(1), Some(5), Some(255)] {
            if t == Tokenizer::PreTokenizedArray && max.is_some() {
                continue;
            }
            let f = full(Analyzer {
                max_token_length: max,
                ..with(t)
            });
            let line = f.encode();
            assert_eq!(FullText::decode(&line), Some(f), "{line}");
            let head = match (t, max) {
                (Tokenizer::WordV1, None) => "v1;",
                (_, None) => "v2;",
                (_, Some(_)) => "v3;",
            };
            assert!(line.starts_with(head), "{line}");
            if let Some(m) = max {
                assert!(line.ends_with(&format!(";{};{m}", t.name())), "{line}");
            } else if t != Tokenizer::WordV1 {
                assert!(line.ends_with(&format!(";{}", t.name())), "{line}");
            }
        }
    }
    // Today's line, byte for byte.
    assert_eq!(
        FullText::default().encode(),
        "v1;english;0000;3f99999a;3f400000"
    );
}

#[test]
fn a_second_encoding_is_refused() {
    let base = "english;0000;3f99999a;3f400000";
    assert!(FullText::decode(&format!("v3;{base};word_v3;5")).is_some());
    for bad in [
        format!("v2;{base};word_v1"),
        format!("v2;{base};word_v5"),
        format!("v3;{base};pre_tokenized_array;5"),
        format!("v3;{base};word_v3"),
        format!("v3;{base};word_v3;0"),
        format!("v3;{base};word_v3;256"),
        format!("v3;{base};word_v3;05"),
        format!("v3;{base};word_v3;+5"),
        format!("v3;{base};word_v3;5.0"),
        format!("v3;{base};word_v3;x"),
        format!("v3;{base};word_v3;5;5"),
        format!("v1;{base};word_v3"),
    ] {
        assert_eq!(FullText::decode(&bad), None, "{bad}");
    }
}

#[test]
fn tokenizer_tables_are_unicode_16() {
    assert_eq!(char::UNICODE_VERSION, (16, 0, 0));
    assert_eq!(unicode_segmentation::UNICODE_VERSION, (16, 0, 0));
    let manifest = include_str!("../../Cargo.toml");
    for krate in ["regex-syntax", "unicode-segmentation"] {
        let line = manifest
            .lines()
            .find(|l| l.starts_with(krate))
            .unwrap_or_else(|| panic!("{krate} is not a dependency"));
        assert!(
            line.contains("\"="),
            "{krate} is not pinned with `=`: {line}"
        );
    }
    assert!(text::is_extended_pictographic('\u{1F600}'));
    assert!(!text::is_extended_pictographic('A'));
    assert!(text::is_ideographic('\u{3007}'));
    assert!(!text::is_ideographic('\u{30BF}'));
    assert!(text::is_regional_indicator('\u{1F1E6}'));
    assert!(!text::is_regional_indicator('A'));
}
