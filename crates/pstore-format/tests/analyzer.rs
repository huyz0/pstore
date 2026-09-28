//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The default analyzer is the analyzer before M14 (M14.1 criterion 1).

use pstore_format::text::{Analyzer, analyze};

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
