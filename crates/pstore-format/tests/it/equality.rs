//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The equality section's format (M230): its own meta-region section, sized from what the
//! block index and the trigram sketch leave, never changing the blocks.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::equality::Equality;
use pstore_format::{Document, Section, Segment, SegmentWriter, Value};

/// Rows whose `tag` repeats every 7 and whose `on` alternates, beside an int.
fn docs(n: usize) -> Vec<Document> {
    (0..n)
        .map(|i| {
            let mut d = Document::new(format!("d{i:05}"), vec![i as f32, 1.0]);
            d.attrs
                .insert("tag".to_owned(), Value::Str(format!("t{}", i % 7)));
            d.attrs.insert("on".to_owned(), Value::Bool(i % 2 == 0));
            d.attrs.insert(
                "s".to_owned(),
                Value::Str(format!("value {i} of {}", i * 7)),
            );
            d.attrs.insert("n".to_owned(), Value::Int(i as i64));
            d
        })
        .collect()
}

fn writer(docs: &[Document], plain: bool) -> SegmentWriter {
    let mut w = SegmentWriter::new(64);
    if plain {
        w = w.without_equality_section_for_test();
    }
    for d in docs {
        w.push(d.clone());
    }
    w
}

async fn open(bytes: bytes::Bytes) -> (MemoryStore, Key, Segment) {
    let s = MemoryStore::new();
    let key = Key::new("seg/1");
    s.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    (s, key, seg)
}

#[tokio::test]
async fn a_segment_without_the_equality_section_answers_as_before() {
    for n in [10, 500, 2000, 20_000] {
        let rows = docs(n);
        let (ps, pk, plain) = open(writer(&rows, true).try_finish().unwrap()).await;
        let (es, ek, with) = open(writer(&rows, false).try_finish().unwrap()).await;
        assert!(plain.section(Section::Equality).is_none(), "{n} rows");
        assert!(with.section(Section::Equality).is_some(), "{n} rows");
        // Same blocks, same zones but for the section, same rows.
        assert_eq!(with.block_count(), plain.block_count(), "{n} rows");
        for (a, b) in with.block_zones().zip(plain.block_zones()) {
            assert_eq!(a.ints, b.ints);
            assert!(a.equality.is_some() && b.equality.is_none());
        }
        let all = |_: &pstore_format::Zones| true;
        assert_eq!(
            with.rows_where(&es, &ek, all).await.unwrap(),
            plain.rows_where(&ps, &pk, all).await.unwrap()
        );
    }
}

#[tokio::test]
async fn the_equality_section_never_narrows_the_trigram_sketch() {
    for n in [10, 500, 2000, 20_000] {
        let rows = docs(n);
        let sketched = |plain| {
            writer(&rows, plain)
                .with_trigram_attrs(&["s".to_owned()])
                .try_finish()
                .unwrap()
        };
        let (_, _, a) = open(sketched(true)).await;
        let (_, _, b) = open(sketched(false)).await;
        assert_eq!(
            a.section(Section::TrigramSketch).map(|r| r.end - r.start),
            b.section(Section::TrigramSketch).map(|r| r.end - r.start),
            "{n} rows"
        );
        assert_eq!(a.block_count(), b.block_count());
    }
}

#[tokio::test]
async fn many_string_attributes_never_change_the_blocks() {
    // 200 attributes, each a value no other row repeats: the section keeps the attributes that
    // fit and drops the rest, and the blocks are the plain segment's.
    let rows: Vec<Document> = (0..2_000)
        .map(|i| {
            let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.0]);
            for a in 0..200 {
                d.attrs
                    .insert(format!("a{a:03}"), Value::Str(format!("{i}-{a}")));
            }
            d
        })
        .collect();
    let bytes = writer(&rows, false).try_finish().unwrap();
    assert!(pstore_format::index_section_len(&bytes).unwrap() <= pstore_format::INDEX_BUDGET);
    let (_, _, with) = open(bytes).await;
    let (_, _, plain) = open(writer(&rows, true).try_finish().unwrap()).await;
    assert_eq!(with.block_count(), plain.block_count());
    let (_, strings, _) = with
        .block_zones()
        .next()
        .unwrap()
        .equality
        .as_ref()
        .unwrap()
        .0
        .shape();
    assert!(
        !strings.is_empty() && strings.len() < 200,
        "{}",
        strings.len()
    );
}

#[tokio::test]
async fn a_filter_that_would_be_mostly_ones_is_not_written() {
    // Blocks of 1,000 rows, each value unique: 1,000 values fill more than a quarter of even
    // the widest filter, so `sku` gets none; `tag`, seven values in every block, prunes nothing
    // a row holds and gets none either. `on` keeps its bits.
    let rows: Vec<Document> = (0..2_000)
        .map(|i| {
            let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.0]);
            d.attrs.insert("sku".into(), Value::Str(format!("s{i}")));
            d.attrs
                .insert("tag".into(), Value::Str(format!("t{}", i % 7)));
            d.attrs.insert("on".into(), Value::Bool(i < 1_000));
            d
        })
        .collect();
    let mut w = SegmentWriter::new(1_000);
    for d in &rows {
        w.push(d.clone());
    }
    let (_, _, seg) = open(w.try_finish().unwrap()).await;
    assert_eq!(seg.block_count(), 2);
    let z = seg.block_zones().next().unwrap();
    let (_, strings, bools) = z.equality.as_ref().unwrap().0.shape();
    assert!(strings.is_empty(), "{strings:?}");
    assert_eq!(bools, vec!["on"]);
}

#[tokio::test]
async fn the_section_never_costs_the_open_a_second_read() {
    // At every budget the plain segment fits, the one with the section fits too, with the same
    // blocks: the section takes only what the index left.
    let rows = docs(200);
    let mut with_section = 0;
    for budget in 100..1_500 {
        let Ok(plain) = writer(&rows, true).with_index_budget(budget).try_finish() else {
            continue;
        };
        let with = writer(&rows, false)
            .with_index_budget(budget)
            .try_finish()
            .unwrap_or_else(|e| panic!("at a budget of {budget}: {e}"));
        let (_, _, a) = open(with).await;
        let (_, _, b) = open(plain).await;
        assert_eq!(a.block_count(), b.block_count(), "at a budget of {budget}");
        with_section += usize::from(a.section(Section::Equality).is_some());
    }
    assert!(with_section > 0, "no budget left room for the section");
}

#[test]
fn a_section_round_trips_and_a_truncated_one_is_refused() {
    let blocks: Vec<Vec<(String, Value)>> = vec![
        vec![
            ("s".into(), Value::Str("x".into())),
            ("b".into(), Value::Bool(true)),
        ],
        vec![("s".into(), Value::DateTime(5))],
        vec![("b".into(), Value::Bool(false))],
    ];
    let built = Equality::build(blocks.len(), usize::MAX, |b| {
        blocks[b].iter().map(|(k, v)| (k.as_str(), v))
    })
    .unwrap();
    let bytes = built.encode();
    assert_eq!(bytes.len(), built.encoded_len());
    assert_eq!(Equality::decode(&bytes).unwrap(), built);
    for cut in 0..bytes.len() {
        assert!(Equality::decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    let mut long = bytes.clone();
    long.push(0);
    assert!(Equality::decode(&long).is_err());
    let s = |v: &str| Value::Str(v.to_owned());
    // Block 0 holds "x" and true; block 1 is open for `s`; block 2 holds neither.
    assert!(built.may_equal("s", 0, &s("x")));
    assert!(!built.may_equal("s", 0, &s("y")));
    assert!(built.may_equal("s", 1, &s("y")));
    assert!(!built.may_equal("s", 2, &s("x")));
    assert!(built.may_equal("b", 0, &Value::Bool(true)));
    assert!(!built.may_equal("b", 0, &Value::Bool(false)));
    assert!(built.may_equal("b", 2, &Value::Bool(false)));
    assert!(!built.may_equal("b", 1, &Value::Bool(true)));
    // An attribute it does not name, and a literal of another kind, rule nothing out.
    assert!(built.may_equal("other", 2, &s("x")));
    assert!(built.may_equal("s", 2, &Value::Int(1)));
}
