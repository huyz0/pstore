//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M260: a full-text schema with `k3` is a line no earlier grammar reads.

use pstore_format::text::{Analyzer, FullText, Language, Tokenizer};

/// One analyzer per line grammar: `v1`, `v2`, `v3`, and `v4:`.
fn analyzers() -> Vec<Analyzer> {
    vec![
        Analyzer::default(),
        Analyzer {
            tokenizer: Tokenizer::WordV3,
            ..Analyzer::default()
        },
        Analyzer {
            tokenizer: Tokenizer::WordV2,
            max_token_length: Some(9),
            ..Analyzer::default()
        },
        Analyzer {
            language: Language::French,
            remove_stopwords: true,
            ..Analyzer::default()
        },
    ]
}

#[test]
fn k3_lines_are_refused_by_older_grammars() {
    for analyzer in analyzers() {
        let without = FullText {
            analyzer,
            ..FullText::default()
        };
        let inner = without.encode();
        assert!(!inner.starts_with("k3:"), "{inner}");
        for k3 in [0.5_f32, 8.0, 1000.0] {
            let with = FullText {
                k3: Some(k3),
                ..without
            };
            let line = with.encode();
            assert_eq!(line, format!("k3:{:08x}:{inner}", k3.to_bits()));
            assert_eq!(FullText::decode(&line), Some(with), "{line}");
            assert_ne!(Some(with), Some(without), "k3 compared");
            // The inner line alone is the field without `k3`.
            assert_eq!(FullText::decode(&inner), Some(without), "{inner}");
        }
        // Out of range, or a second spelling of an in-range value, is refused.
        for bad in [0.0_f32, -1.0, 1000.5, f32::NAN, f32::INFINITY] {
            let line = format!("k3:{:08x}:{inner}", bad.to_bits());
            assert_eq!(FullText::decode(&line), None, "{line}");
        }
        for spelling in [
            format!("k3:{:08X}:{inner}", 1.2_f32.to_bits()),
            format!("k3:+{:07x}:{inner}", 8.0_f32.to_bits() & 0x0fff_ffff),
            format!("k3:{:x}:{inner}", 0x0080_0000_u32),
            format!(
                "k3:{:08x}:k3:{:08x}:{inner}",
                8.0_f32.to_bits(),
                8.0_f32.to_bits()
            ),
            format!("k3:{:08x}:", 8.0_f32.to_bits()),
            format!("k3:{:08x}", 8.0_f32.to_bits()),
        ] {
            assert_eq!(FullText::decode(&spelling), None, "{spelling}");
        }
    }
    // A field without `k3` keeps its pre-M260 line.
    let f = FullText::default();
    assert_eq!(
        f.encode(),
        format!(
            "v1;english;0000;{:08x};{:08x}",
            f.k1.to_bits(),
            f.b.to_bits()
        )
    );
}
