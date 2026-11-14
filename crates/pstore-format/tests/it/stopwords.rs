//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M204: stopwords beyond English -- Snowball's lists, golden streams, and their encoding.

use pstore_format::text::{Analyzer, FullText, Language, Tokenizer, analyze, analyze_query};

/// `(language, sentence, its terms once stopwords are dropped, unstemmed)`, written by hand:
/// each dropped word was looked up in the language's Snowball list, each kept one too.
const GOLDEN: [(Language, &str, &[&str]); 12] = [
    (
        Language::Danish,
        "Jeg og hun har en hund i huset",
        &["hund", "huset"],
    ),
    (
        Language::Dutch,
        "De kat slaapt op de mat met een hoed",
        &["kat", "slaapt", "mat", "hoed"],
    ),
    (
        Language::Finnish,
        "Minä olen kotona ja kissa nukkuu",
        &["kotona", "kissa", "nukkuu"],
    ),
    (
        Language::French,
        "Le chat dort sur la table avec une souris",
        &["chat", "dort", "table", "souris"],
    ),
    (
        Language::German,
        "Der Hund und die Katze spielen im Garten",
        &["hund", "katze", "spielen", "garten"],
    ),
    (
        Language::Hungarian,
        "A macska és a kutya a kertben játszik",
        &["macska", "kutya", "kertben", "játszik"],
    ),
    (
        Language::Italian,
        "Il gatto e il cane sono nel giardino",
        &["gatto", "cane", "giardino"],
    ),
    (
        Language::Norwegian,
        "Hunden og katten er i hagen",
        &["hunden", "katten", "hagen"],
    ),
    (
        Language::Portuguese,
        "O gato e o cão estão no jardim",
        &["gato", "cão", "jardim"],
    ),
    (
        Language::Russian,
        "Кошка и собака были в саду",
        &["кошка", "собака", "саду"],
    ),
    (
        Language::Spanish,
        "El gato estaba con el perro en el jardín",
        &["gato", "perro", "jardín"],
    ),
    (
        Language::Swedish,
        "Katten och hunden är i trädgården",
        &["katten", "hunden", "trädgården"],
    ),
];

fn of(language: Language, remove_stopwords: bool) -> Analyzer {
    Analyzer {
        language,
        remove_stopwords,
        ..Analyzer::default()
    }
}

/// Criterion 1: each language's golden stream, unstemmed and stemmed; and without
/// `remove_stopwords` every word is kept.
#[test]
fn stopwords_drop_each_languages_list() {
    for (language, sentence, golden) in GOLDEN {
        let kept = analyze(&of(language, true), sentence);
        assert_eq!(kept, golden, "{language:?}");
        let all = analyze(&of(language, false), sentence);
        assert_eq!(
            all.len(),
            sentence.split_whitespace().count(),
            "{language:?}: nothing dropped without the option"
        );
        assert!(
            all.len() > golden.len(),
            "{language:?}: the sentence holds stopwords"
        );
        // Stemmed: the golden words' stems -- dropped before stemming, so a stopword whose
        // stem is not on the list (Spanish `estaba`, `estab`) is dropped all the same.
        let stemmed = analyze(
            &Analyzer {
                stemming: true,
                ..of(language, true)
            },
            sentence,
        );
        let stems: Vec<String> = golden
            .iter()
            .map(|w| {
                analyze(
                    &Analyzer {
                        stemming: true,
                        ..of(language, false)
                    },
                    w,
                )
                .concat()
            })
            .collect();
        assert_eq!(stemmed, stems, "{language:?} stemmed");
        // A query drops the same words a document does: one pipeline.
        assert_eq!(analyze_query(&of(language, true), sentence), golden);
    }
    let spanish = Analyzer {
        stemming: true,
        ..of(Language::Spanish, false)
    };
    assert_eq!(
        analyze(&spanish, "estaba"),
        ["estab"],
        "the stem is not on the list"
    );
}

/// Criterion 2: every list word is dropped, alone, folded or not; its uppercase too unless
/// case-sensitive. Each list's distinct words are counted.
#[test]
fn every_list_word_is_a_stopword() {
    let counts = [
        (Language::Danish, 94),
        (Language::Dutch, 101),
        (Language::Finnish, 229),
        (Language::French, 154),
        (Language::German, 231),
        (Language::Hungarian, 198),
        (Language::Italian, 279),
        (Language::Norwegian, 172),
        (Language::Portuguese, 203),
        (Language::Russian, 159),
        (Language::Spanish, 308),
        (Language::Swedish, 114),
    ];
    for (language, count) in counts {
        let words = language.stopwords().unwrap();
        let distinct: std::collections::BTreeSet<&str> = words.iter().copied().collect();
        assert_eq!(distinct.len(), count, "{language:?}");
        for w in distinct {
            if w == "ill." {
                continue;
            }
            for ascii_folding in [false, true] {
                let a = Analyzer {
                    ascii_folding,
                    ..of(language, true)
                };
                assert!(
                    analyze(&a, w).is_empty(),
                    "{language:?} {w:?} {ascii_folding}"
                );
                if w != "daß" {
                    assert!(
                        analyze(&a, &w.to_uppercase()).is_empty(),
                        "{language:?} {w:?}"
                    );
                }
            }
            let cased = Analyzer {
                case_sensitive: true,
                ..of(language, true)
            };
            let upper = w.to_uppercase();
            if upper != w {
                assert_eq!(analyze(&cased, &upper), vec![upper], "{language:?} {w:?}");
            }
        }
    }
    for language in [
        Language::Arabic,
        Language::Greek,
        Language::Romanian,
        Language::Tamil,
        Language::Turkish,
    ] {
        assert!(language.stopwords().is_none(), "{language:?}");
    }
    let source = include_str!("../../src/stopwords.rs");
    assert!(source.contains("Copyright (c) 2001, Dr Martin Porter"));
    assert!(source.contains("Neither the name of the Snowball project"));
}

/// Criterion 3: English is Lucene's 33 words, and a sentence analyzes as it did.
#[test]
fn english_stopwords_are_lucenes() {
    let lucene = [
        "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is",
        "it", "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there",
        "these", "they", "this", "to", "was", "will", "with",
    ];
    assert_eq!(Language::English.stopwords().unwrap(), lucene);
    let a = of(Language::English, true);
    assert_eq!(
        analyze(&a, "The fox is in the den with their cubs and others"),
        ["fox", "den", "cubs", "others"]
    );
}

/// Criterion 4: a list word is folded as the token is.
#[test]
fn folded_stopwords_match_folded_tokens() {
    for ascii_folding in [false, true] {
        let a = Analyzer {
            ascii_folding,
            ..of(Language::French, true)
        };
        assert!(analyze(&a, "À était").is_empty(), "folding {ascii_folding}");
    }
    // Folding brings `où` ("where") onto the list's `ou` ("or").
    let folded = Analyzer {
        ascii_folding: true,
        ..of(Language::French, true)
    };
    assert!(analyze(&folded, "où").is_empty());
    assert_eq!(analyze(&of(Language::French, true), "où"), ["où"]);
}

/// Criterion 5: a new list is behind `v4:`, an older line is unchanged, and each analyzer has
/// one spelling.
#[test]
fn stopword_lines_are_refused_by_older_grammars() {
    for language in Language::ALL {
        for remove_stopwords in [false, true] {
            for (tokenizer, max) in [
                (Tokenizer::WordV1, None),
                (Tokenizer::WordV3, None),
                (Tokenizer::WordV2, Some(9)),
            ] {
                let f = FullText {
                    analyzer: Analyzer {
                        tokenizer,
                        max_token_length: max,
                        ..of(language, remove_stopwords)
                    },
                    ..FullText::default()
                };
                let line = f.encode();
                let new = remove_stopwords && language != Language::English;
                assert_eq!(line.starts_with("v4:"), new, "{line}");
                if remove_stopwords && language.stopwords().is_none() {
                    assert_eq!(FullText::decode(&line), None, "no list: {line}");
                    continue;
                }
                assert_eq!(FullText::decode(&line), Some(f), "{line}");
                if new {
                    let bare = line.strip_prefix("v4:").unwrap();
                    assert_eq!(FullText::decode(bare), None, "bare {bare}");
                } else {
                    let prefixed = format!("v4:{line}");
                    assert_eq!(FullText::decode(&prefixed), None, "prefixed {prefixed}");
                }
            }
        }
    }
    // English with stopwords: the line before M204, byte for byte.
    let english = FullText {
        analyzer: of(Language::English, true),
        ..FullText::default()
    };
    assert_eq!(
        english.encode(),
        format!(
            "v1;english;0100;{:08x};{:08x}",
            english.k1.to_bits(),
            english.b.to_bits()
        )
    );
}
