//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The trigram sketch's format (M15.2): its own meta-region section, sized from what the block
//! index leaves, absent when nothing is declared.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::trigram::{Sketch, fold};
use pstore_format::{Document, Section, Segment, SegmentWriter, Value};

fn docs(n: usize) -> Vec<Document> {
    (0..n)
        .map(|i| {
            let mut d = Document::new(format!("d{i:05}"), vec![i as f32, 1.0]);
            d.attrs.insert(
                "s".to_owned(),
                Value::Str(format!("value {i} of {}", i * 7)),
            );
            d.attrs.insert("n".to_owned(), Value::Int(i as i64));
            d
        })
        .collect()
}

fn write(n: usize, declared: Option<&[&str]>) -> bytes::Bytes {
    let mut w = SegmentWriter::new(64);
    if let Some(names) = declared {
        let names: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
        w = w.with_trigram_attrs(&names);
    }
    for d in docs(n) {
        w.push(d);
    }
    w.finish()
}

async fn open(bytes: bytes::Bytes) -> Segment {
    let s = MemoryStore::new();
    let key = Key::new("seg/1");
    s.put(&key, bytes).await.unwrap();
    Segment::open(&s, &key).await.unwrap()
}

#[tokio::test]
async fn a_declaration_never_changes_the_block_layout() {
    for n in [10, 500, 2000, 20_000] {
        let plain = open(write(n, None)).await;
        let sketched = open(write(n, Some(&["s"]))).await;
        assert_eq!(sketched.block_count(), plain.block_count(), "{n} rows");
    }
    // Small enough to have room: the section is there.
    assert!(
        open(write(500, Some(&["s"])))
            .await
            .section(Section::TrigramSketch)
            .is_some()
    );
}

#[tokio::test]
async fn nothing_declared_writes_what_m14_wrote() {
    let before = write(500, None);
    assert_eq!(write(500, Some(&[])), before);
    assert!(open(before).await.section(Section::TrigramSketch).is_none());
}

#[test]
fn the_fold_is_simple_case_folding() {
    for class in [
        &['s', 'S', 'ſ'][..],
        &['σ', 'ς', 'Σ'],
        &['µ', 'μ', 'Μ'],
        &['k', 'K', '\u{212A}'],
    ] {
        for c in class {
            assert_eq!(fold(*c), fold(class[0]), "{c:?}");
        }
    }
    assert_ne!(fold('İ'), fold('i'));
    assert_ne!(fold('a'), fold('b'));
    assert_eq!(fold('/'), '/');
}

#[test]
fn a_sketch_round_trips_and_a_truncated_one_is_refused() {
    let values = [Some("abcd"), None, Some("ſtar")];
    let sketch = Sketch::build(&["s".to_owned()], 3, 256, |attr, block| {
        assert_eq!(attr, "s");
        values[block].map(str::to_owned).into_iter().collect()
    });
    let bytes = sketch.encode();
    assert_eq!(Sketch::decode(&bytes).unwrap(), sketch);
    for cut in 0..bytes.len() {
        assert!(Sketch::decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    // A block without the attribute holds nothing, so any requirement rules it out.
    let tri = pstore_format::trigram::trigrams("abc");
    assert!(sketch.may_hold("s", 0, &tri));
    assert!(!sketch.may_hold("s", 1, &tri));
    // Folded: `STAR` holds what `ſtar` does.
    assert!(sketch.may_hold("s", 2, &pstore_format::trigram::trigrams("STAR")));
}
