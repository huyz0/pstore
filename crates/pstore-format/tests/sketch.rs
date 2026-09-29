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

#[test]
fn a_sketch_never_costs_the_open_a_second_read() {
    // Every budget at which the undeclared segment fits the one suffix read, the declared one
    // fits too, with the same blocks: the sketch takes only what the index left, less its own
    // directory entry. Stepped a byte at a time, so the window a missing entry opens is hit.
    let build = |budget: usize, declared: bool| {
        let mut w = SegmentWriter::new(64).with_index_budget(budget);
        if declared {
            w = w.with_trigram_attrs(&["s".to_owned()]);
        }
        for d in docs(200) {
            w.push(d);
        }
        w.try_finish()
    };
    let mut sketched = 0;
    for budget in 100..1_500 {
        let Ok(plain) = build(budget, false) else {
            continue;
        };
        let declared =
            build(budget, true).unwrap_or_else(|e| panic!("at a budget of {budget}: {e}"));
        let has = declared.len() != plain.len();
        sketched += usize::from(has);
        if !has {
            assert_eq!(declared, plain, "at a budget of {budget}");
        }
    }
    assert!(sketched > 0, "no budget left room for a sketch");
}

#[test]
fn a_sketch_is_as_long_as_its_encoded_len_says() {
    // The writer sizes the sketch by `encoded_len` without building it, so the two must agree.
    for attrs in [vec!["s"], vec!["a", "longer_name", "é"]] {
        let attrs: Vec<String> = attrs.iter().map(|s| (*s).to_owned()).collect();
        for blocks in [1, 3, 7] {
            for bits in [64, 128, 1024, 2048] {
                let built = Sketch::build(&attrs, blocks, bits, |_, _| vec!["abcd".to_owned()]);
                assert_eq!(
                    built.encode().len(),
                    Sketch::encoded_len(&attrs, blocks, bits),
                    "{attrs:?} {blocks} {bits}"
                );
            }
        }
    }
}

#[test]
fn a_trigram_sets_the_bits_fnv_1a_names() {
    // The hash is part of the format: a sketch written by one build is read by the next. Its
    // bits are checked against FNV-1a 64 computed here, over the folded trigram's UTF-8,
    // with the offset basis XORed with each seed.
    let fnv = |seed: u64, bytes: &[u8]| {
        let mut h = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    };
    // 2,048 bits, so the positions keep eleven bits of the hash rather than six.
    for (value, folded) in [("abc", "ABC"), ("ſtä", "STÄ")] {
        let sketch = Sketch::build(&["s".to_owned()], 1, 2048, |_, _| vec![value.to_owned()]);
        let bytes = sketch.encode();
        // The count, the name, `bits` and the block count, then the one 256-byte filter.
        let filter = &bytes[17..];
        assert_eq!(filter.len(), 256);
        let set: Vec<u64> = (0..2048)
            .filter(|p| filter[p / 8] & (1 << (p % 8)) != 0)
            .map(|p| p as u64)
            .collect();
        let mut want: Vec<u64> = [0u64, 0x9e37_79b9]
            .iter()
            .map(|seed| fnv(*seed, folded.as_bytes()) % 2048)
            .collect();
        want.sort_unstable();
        want.dedup();
        assert_eq!(set, want, "{value}");
    }
}
