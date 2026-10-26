//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The default analyzer is the analyzer before M14 (M14.1 criterion 1).

use pstore_format::text::{Analyzer, FullText, Language, analyze};

/// `analyze` as it stood before M14, copied rather than called.
fn pre_m14(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[test]
fn the_default_analyzer_is_the_pre_m14_split() {
    let d = Analyzer::default();
    for s in [
        "",
        "Quarterly REVENUE, report!",
        "don't stop -- the_end 2024-09-28",
        "Café au lait, naïve Straße, ÆON",
        "Привет мир, Grüße aus Köln",
        "東京 タワー 123abc",
        "the and of a an",
        "Running runs RAN",
        "tabs\tand\nnewlines",
        "ǅemal İstanbul ﬁne",
    ] {
        assert_eq!(analyze(&d, s), pre_m14(s), "{s:?}");
    }
}

#[test]
fn each_step_runs_in_its_order() {
    let mut a = Analyzer {
        stemming: true,
        ..Analyzer::default()
    };
    assert_eq!(analyze(&a, "Running runs"), ["run", "run"]);
    a.remove_stopwords = true;
    assert_eq!(analyze(&a, "the cats of Rome"), ["cat", "rome"]);
    // Case first: a case-sensitive index keeps `The` past the lowercase stopword list.
    a.case_sensitive = true;
    assert_eq!(analyze(&a, "The the"), ["The"]);
    let f = Analyzer {
        ascii_folding: true,
        ..Analyzer::default()
    };
    assert_eq!(
        analyze(&f, "Café naïve Straße"),
        ["cafe", "naive", "straße"]
    );
}

#[test]
fn every_language_stems_with_its_own_algorithm() {
    // A language mapped to another's stemmer still stems, so nothing fails loudly: an index
    // declared Danish would quietly be English. Each language must stem this mixed vocabulary
    // differently from every other, and its wire name must read back as itself.
    let words = [
        "running",
        "connections",
        "kærligheden",
        "huizen",
        "talossa",
        "maisons",
        "häuser",
        "ανθρώπους",
        "házakban",
        "case",
        "husene",
        "cidades",
        "orașele",
        "городами",
        "ciudades",
        "husen",
        "வீடுகள்",
        "evlerde",
        "مكتبات",
        "bilerne",
        "nations",
    ];
    let mut seen: Vec<(Language, Vec<String>)> = Vec::new();
    for language in Language::ALL {
        assert_eq!(Language::parse(language.name()), Some(language));
        let a = Analyzer {
            language,
            stemming: true,
            ..Analyzer::default()
        };
        let stems: Vec<String> = words.iter().flat_map(|w| analyze(&a, w)).collect();
        for (other, theirs) in &seen {
            assert_ne!(&stems, theirs, "{language:?} stems as {other:?} does");
        }
        seen.push((language, stems));
    }
    assert_eq!(Language::parse("klingon"), None);
}

#[test]
fn a_damaged_full_text_line_is_refused() {
    // The line is stored in a schema and read back on every open, so a damaged one must be
    // refused rather than read as some other analyzer.
    let good = FullText::default().encode();
    assert_eq!(FullText::decode(&good), Some(FullText::default()));
    let parts: Vec<&str> = good.split(';').collect();
    let with = |i: usize, s: &str| {
        let mut p = parts.clone();
        p[i] = s;
        p.join(";")
    };
    for bad in [
        with(0, "v2"),
        with(1, "klingon"),
        with(2, "0102"),
        with(2, "010"),
        with(2, "01010"),
        with(3, "zz"),
        with(4, "zz"),
        format!("{good};extra"),
        String::new(),
    ] {
        assert_eq!(FullText::decode(&bad), None, "{bad:?}");
    }
    for cut in 0..parts.len() {
        assert_eq!(
            FullText::decode(&parts[..cut].join(";")),
            None,
            "{cut} parts"
        );
    }
}
