//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Which attribute a segment's text index was built over — M6c.1.
//!
//! ⚠️ The failure being pinned is **not** a missing feature. `run.rs` refuses a text query
//! naming anything but `text`, and that refusal is correct only while every segment is built
//! over `text`. A segment that carries the name is what lets the reader keep being right.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, Segment, SegmentWriter, Value, VectorField, text};
use std::collections::BTreeMap;

fn doc(i: usize, attr: &str, body: &str) -> Document {
    Document {
        id: format!("d{i}"),
        vectors: BTreeMap::from([(
            "v".to_owned(),
            VectorField::Dense(vec![vec![i as f32, 1.0]]),
        )]),
        attrs: BTreeMap::from([(attr.to_owned(), Value::Str(body.to_owned()))]),
    }
}

fn corpus(attr: &str) -> Vec<Document> {
    (0..12)
        .map(|i| doc(i, attr, &format!("row {i} carries a quantum of prose")))
        .collect()
}

/// Seals `docs`, indexing `attr` as the text field, and opens the result.
async fn round_trip(docs: &[Document], field: Option<&str>) -> (Segment, MemoryStore, Key) {
    let s = MemoryStore::new();
    let key = Key::new("seg");
    let mut w = SegmentWriter::new(4);
    for d in docs {
        w.push(d.clone());
    }
    if let Some(f) = field {
        let built = text::build(docs, f);
        w = w
            .with_section(Section::TextPostings, built.postings)
            .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
            .with_text_fields(&[f.to_owned()]);
    }
    s.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    (seg, s, key)
}

#[tokio::test]
async fn a_segment_records_the_text_field_it_indexed() {
    let (seg, _, _) = round_trip(&corpus("body"), Some("body")).await;
    assert_eq!(seg.text_fields(), ["body"]);
    assert!(
        seg.section(Section::TextFields).is_some(),
        "the name came from the fallback, not from a recorded section"
    );

    // ⚠️ The default is recorded too, and read back FROM the section. A writer that skipped
    // the section for the default name would leave the common path untested and make absence
    // mean two different things in a newly written segment.
    let (seg, _, _) = round_trip(&corpus("text"), Some(text::DEFAULT_TEXT_FIELD)).await;
    assert_eq!(seg.text_fields(), ["text"]);
    assert!(seg.section(Section::TextFields).is_some());
}

#[tokio::test]
async fn a_segment_without_the_section_reads_as_the_old_default() {
    let docs = corpus("text");
    let s = MemoryStore::new();
    let key = Key::new("seg");
    let mut w = SegmentWriter::new(4);
    for d in &docs {
        w.push(d.clone());
    }
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let bytes = w
        .with_section(Section::TextPostings, built.postings)
        .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
        .with_text_fields(&["text".to_owned()])
        .without_text_fields_section_for_test()
        .try_finish()
        .unwrap();
    s.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();

    assert!(
        seg.section(Section::TextFields).is_none(),
        "the fixture is meant to be a segment written before the section existed"
    );
    // ⚠️ Absence is not "no text field". Reading it that way turns off the text index of
    // every segment written before this milestone, which is all of them.
    assert_eq!(seg.text_fields(), ["text"]);
    assert!(seg.has_text());
}

#[tokio::test]
async fn a_segment_without_text_writes_no_text_fields_section() {
    let (seg, _, _) = round_trip(&corpus("body"), None).await;
    assert!(!seg.has_text());
    assert!(
        seg.section(Section::TextFields).is_none(),
        "a vector-only segment paid for a table describing an index it does not carry"
    );
    assert!(
        seg.text_fields().is_empty(),
        "a segment with no postings claimed a text field"
    );
}

#[tokio::test]
async fn three_meta_entries_still_resolve_to_their_own_bytes() {
    // Two named vector fields force a `Fields` table, the text index forces `TextFields`,
    // and the block index is always there: three entries patched inside the meta region.
    let docs: Vec<Document> = (0..12)
        .map(|i| Document {
            id: format!("d{i}"),
            vectors: BTreeMap::from([
                (
                    "alpha".to_owned(),
                    VectorField::Dense(vec![vec![i as f32, 1.0]]),
                ),
                (
                    "beta".to_owned(),
                    VectorField::Dense(vec![vec![2.0, i as f32]]),
                ),
            ]),
            attrs: BTreeMap::from([(
                "body".to_owned(),
                Value::Str(format!("row {i} carries a quantum of prose")),
            )]),
        })
        .collect();
    let (seg, s, key) = round_trip(&docs, Some("body")).await;

    // ⚠️ Every meta-region entry, read for its OWN bytes. The patch loop hands each entry an
    // offset computed by hand; a wrong slot returns one section's bytes as another's and
    // decodes without complaint.
    assert_eq!(seg.text_fields(), ["body"]);
    assert_eq!(
        seg.fields()
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"],
        "the Fields table decoded as something else"
    );
    let out = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(out.len(), 12, "the block index decoded as something else");
    assert_eq!(out[7].field("beta"), [vec![2.0, 7.0]]);
    assert!(seg.has_text());
}
