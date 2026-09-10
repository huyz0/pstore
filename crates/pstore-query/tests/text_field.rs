//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A text query is checked against the field the **segment** carries — M6c.2.
//!
//! ⚠️ `run.rs` refused anything but `text` against the constant, and that was right: *"a
//! request naming another would otherwise be answered with the `text` field's ranking and
//! nothing anywhere would say so"*. It stops being right the moment a segment is built over
//! something else, and then the same comparison produces exactly the wrong answer it was
//! written to prevent. The check has to ask the segment.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, FormatError, Value, text};
use pstore_index::cluster::Params;
use pstore_index::vec_index;
use pstore_query::{Fusion, Prefetch, QueryError, query};

const SEG: &str = "t/idx/text.seg";
const CEN: &str = "t/idx/text.centroids";
const DIM: usize = 8;

fn doc(i: usize, attr: &str) -> Document {
    let mut d = Document::new(
        format!("d{i:05}"),
        (0..DIM).map(|j| ((i + j) % 11) as f32 / 11.0).collect(),
    );
    d.attrs.insert(
        attr.to_owned(),
        Value::Str(format!("document {i} quarterly revenue w{}", i % 29)),
    );
    d
}

/// A segment whose text index is built over `attr`, with its term dictionary beside it.
async fn put(store: &MemoryStore, attr: &str) -> (Key, Key) {
    let docs: Vec<Document> = (0..120).map(|i| doc(i, attr)).collect();
    let built = vec_index::build_all(
        &docs,
        Params {
            target_list_size: 40,
            exact_scan_threshold: 200,
            ..Params::default()
        },
        pstore_format::DEFAULT_FIELD,
        None,
        Some(attr),
    );
    let (seg, cen) = (Key::new(SEG), Key::new(CEN));
    store.put(&seg, built.segment).await.unwrap();
    if let Some(c) = &built.centroids {
        store
            .put(&cen, bytes::Bytes::from(c.encode()))
            .await
            .unwrap();
    }
    store
        .put(
            &text::dict_key(&seg),
            bytes::Bytes::from(built.text_dictionary.expect("no term dictionary")),
        )
        .await
        .unwrap();
    (seg, cen)
}

fn text_leg(field: &str) -> Vec<Prefetch> {
    vec![Prefetch::Text {
        field: field.to_owned(),
        query: "quarterly revenue".to_owned(),
        limit: 10,
    }]
}

async fn run(store: &MemoryStore, seg: &Key, cen: &Key, field: &str) -> Result<usize, QueryError> {
    query(
        store,
        seg,
        cen,
        &text_leg(field),
        Fusion::Rrf { k: 60.0 },
        10,
    )
    .await
    .map(|h| h.len())
}

#[tokio::test]
async fn a_segment_built_over_a_named_attribute_answers_that_name() {
    let s = MemoryStore::new();
    let (seg, cen) = put(&s, "body").await;
    let n = run(&s, &seg, &cen, "body").await.unwrap();
    assert!(
        n > 0,
        "a text leg over the field the segment carries found nothing"
    );
}

#[tokio::test]
async fn a_query_naming_the_wrong_text_field_is_refused() {
    // ⚠️ Both directions. The one that matters is the second: before M6c.2 a `text` query
    // against a `body` segment was ACCEPTED and answered with `body`'s ranking under the
    // name `text` — a confident wrong answer, which is worse than an empty one.
    let s = MemoryStore::new();
    let (seg, cen) = put(&s, text::DEFAULT_TEXT_FIELD).await;
    let e = run(&s, &seg, &cen, "body").await.unwrap_err();
    assert!(
        matches!(e, QueryError::Format(FormatError::UnknownField)),
        "a query for `body` against a `text` segment was not refused: {e:?}"
    );

    let s = MemoryStore::new();
    let (seg, cen) = put(&s, "body").await;
    let e = run(&s, &seg, &cen, text::DEFAULT_TEXT_FIELD)
        .await
        .unwrap_err();
    assert!(
        matches!(e, QueryError::Format(FormatError::UnknownField)),
        "a query for `text` against a `body` segment was not refused: {e:?}"
    );
}

#[tokio::test]
async fn a_pre_m6c_segment_still_answers_the_default() {
    // ⚠️ A segment written before the section existed, built through the writer with the
    // section suppressed rather than byte-patched: a hand-edited directory would be a fixture
    // testing the fixture. Reading its absence as "no text field" would refuse every text
    // query against every segment written so far, which is all of them.
    let s = MemoryStore::new();
    let docs: Vec<Document> = (0..120).map(|i| doc(i, text::DEFAULT_TEXT_FIELD)).collect();
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let mut w = pstore_format::SegmentWriter::new(32);
    for d in &docs {
        w.push(d.clone());
    }
    let bytes = w
        .with_section(pstore_format::Section::TextPostings, built.postings)
        .with_section(
            pstore_format::Section::Fieldnorms,
            text::encode_norms(&built.fieldnorms),
        )
        .with_text_fields(&[text::DEFAULT_TEXT_FIELD.to_owned()])
        .without_text_fields_section_for_test()
        .try_finish()
        .unwrap();
    let (seg, cen) = (Key::new(SEG), Key::new(CEN));
    s.put(&seg, bytes).await.unwrap();
    s.put(&text::dict_key(&seg), bytes::Bytes::from(built.dictionary))
        .await
        .unwrap();

    let n = run(&s, &seg, &cen, text::DEFAULT_TEXT_FIELD).await.unwrap();
    assert!(
        n > 0,
        "a pre-M6c segment stopped answering its own text field"
    );
}
