//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The writer's two standing guards, both of which a mutation sweep found untested.
//!
//! ⚠️ Neither is new code. Both shipped in earlier milestones and both survived mutation:
//! `writer.rs`'s sparse-postings refusal survived `&& -> ||`, and the index budget survived
//! `- -> +`. A guard nothing constrains is a guard that can be deleted by accident.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{
    Document, INDEX_BUDGET, Impact, SUFFIX_FETCH, Section, Segment, SegmentWriter, Value,
    VectorField,
};
use pstore_types::TenantId;
use std::collections::BTreeMap;

#[test]
fn a_sparse_document_is_refused_even_when_another_section_is_attached() {
    // The refusal exists because the postings are built a layer up: a writer handed a sparse
    // document and no `SparsePostings` section would store the document with its field
    // silently missing, which is the loss `check_storable` guards the other door against.
    //
    // ⚠️ The other section is the point of the fixture. The guard reads
    // `*s == Section::SparsePostings && !b.is_empty()` over everything attached, and with
    // nothing else attached `&&` and `||` agree on every input — which is why `&& -> ||`
    // survived every test in the tree. With a non-sparse section present, `||` is satisfied
    // by that section and the refusal disappears.
    let mut d = Document::new("d0".to_owned(), vec![1.0, 2.0]);
    d.vectors.insert(
        "body_sparse".to_owned(),
        VectorField::Sparse(vec![(3, Impact::new(0.5))]),
    );

    let mut w = SegmentWriter::new(4);
    w.push(d.clone());
    let err = w
        .with_section(Section::RaBitQ, vec![0u8; 8])
        .try_finish()
        .expect_err("a sparse field was accepted with no postings attached");
    assert!(
        format!("{err}").contains("SparsePostings"),
        "the refusal did not name what was missing: {err}"
    );

    // ⚠️ And an EMPTY postings section is not a postings section. It is what a builder that
    // ran over the wrong field name produces, and accepting it stores the document with its
    // sparse field gone.
    let mut w = SegmentWriter::new(4);
    w.push(d);
    assert!(
        w.with_section(Section::SparsePostings, Vec::new())
            .try_finish()
            .is_err(),
        "an empty SparsePostings section was accepted as postings"
    );
}

#[test]
fn the_index_budget_leaves_room_for_the_footer() {
    // ⚠️ Not arithmetic restated. The footer and the index section share **one** suffix read,
    // so a budget equal to or larger than that read admits a segment whose meta region cannot
    // arrive with its own footer — and `Segment::open` then costs a second, sequential round
    // trip on every cold open, which is the difference between a three-hop query and a
    // four-hop one. `- -> +` in the budget's definition is exactly that mistake and nothing
    // in the tree said so.
    assert!(
        INDEX_BUDGET < SUFFIX_FETCH as usize,
        "the index budget ({INDEX_BUDGET}) does not fit inside the suffix read \
         ({SUFFIX_FETCH}), so the widest accepted segment cannot open in one read"
    );
}

#[tokio::test]
async fn a_segment_at_the_budget_opens_in_one_read() {
    // The behavioural half: the widest segment this writer will emit still opens in one
    // round trip. The fitting loop doubles the block size until the meta region fits the
    // budget, so a wide index is the case that exercises the bound rather than the fixture.
    let docs: Vec<Document> = (0..600)
        .map(|i| Document {
            id: format!("d{i:05}"),
            vectors: BTreeMap::new(),
            attrs: (0..6)
                .map(|k| (format!("attribute_number_{k:02}"), Value::Int(i as i64 * k)))
                .collect(),
        })
        .collect();
    let mut w = SegmentWriter::new(1);
    for d in &docs {
        w.push(d.clone());
    }
    let bytes = w.try_finish().unwrap();
    let idx_len = pstore_format::index_section_len(&bytes).expect("no footer");

    let t = TenantId(31);
    let store = Accounted::new(MemoryStore::new());
    let s = store.as_tenant(t);
    let key = Key::new("wide.seg");
    s.put(&key, bytes).await.unwrap();
    let before = store.count(t, OpClass::Read);

    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(seg.row_count(), 600);
    assert_eq!(
        store.count(t, OpClass::Read) - before,
        1,
        "opening the widest segment the writer emits cost more than the one suffix read"
    );
    assert!(
        idx_len <= INDEX_BUDGET,
        "the writer emitted a meta region of {idx_len} bytes, above its own budget of \
         {INDEX_BUDGET}"
    );
}
