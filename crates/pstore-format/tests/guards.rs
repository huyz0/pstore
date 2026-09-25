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

/// `n` documents over `rows_per_block`-row blocks, the integer attribute of row `i` named by
/// `name(i)` and valued `i`.
async fn with_int_names(
    n: usize,
    rows_per_block: usize,
    name: impl Fn(usize) -> String,
) -> (
    MemoryStore,
    Key,
    Result<bytes::Bytes, pstore_format::FormatError>,
) {
    let mut w = SegmentWriter::new(rows_per_block);
    for i in 0..n {
        let mut d = Document::new(format!("d{i:04}"), vec![i as f32, 1.0]);
        d.attrs.insert(name(i), Value::Int(i as i64));
        w.push(d);
    }
    (
        MemoryStore::new(),
        Key::new("wide/names.seg"),
        w.try_finish(),
    )
}

#[tokio::test]
async fn distinct_integer_names_past_the_budget_seal_without_zone_maps() {
    // ⚠️ M9a, found at spec review. Every integer attribute's NAME is a zone-map key in the
    // block index, and doubling the block size cannot shrink a union of names. Before the
    // fallback this segment was refused -- and the fold seals with the same writer, so one
    // batch would have failed every fold of its tenant.
    let (s, key, bytes) = with_int_names(300, 16, |i| format!("score_{i:020}")).await;
    s.put(&key, bytes.expect("the segment was refused"))
        .await
        .unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(seg.row_count(), 300);
    // From the requested block size, not collapsed to one block: one block would make
    // resolving a single hit read the whole data section.
    assert_eq!(seg.block_count(), 300usize.div_ceil(16));
    // No zone maps, so no block can be ruled out -- by the one filter every block but one
    // would otherwise prune.
    let only = pstore_format::Filter::Gt("score_00000000000000000005".to_owned(), 4);
    assert_eq!(seg.blocks_to_read(Some(&only)).len(), seg.block_count());
    let rows = seg.scan(&s, &key, Some(&only)).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "d0005");
}

#[tokio::test]
async fn a_segment_that_fits_keeps_its_zone_maps() {
    let (s, key, bytes) = with_int_names(300, 16, |_| "n".to_owned()).await;
    s.put(&key, bytes.unwrap()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(seg.block_count(), 300usize.div_ceil(16));
    let tail = pstore_format::Filter::Gt("n".to_owned(), 290);
    assert_eq!(seg.blocks_to_read(Some(&tail)), vec![seg.block_count() - 1]);
}

#[tokio::test]
async fn a_zone_free_index_that_still_does_not_fit_keeps_doubling() {
    // The fallback reruns the doubling. The budget leaves the block index 64 bytes beside the
    // rest of the meta region: zone-free at 16 rows a block (19 entries of 20 bytes, plus a
    // 4-byte count) does not fit either; at 128 rows a block (3 entries, 64 bytes) it does.
    // The rest of the region is measured, from the same documents at one zone-free block
    // (a 24-byte index), so the fixture does not hard-code the directory's size.
    let one_block = {
        let mut w = SegmentWriter::new(1_000);
        for i in 0..300usize {
            w.push(Document::new(format!("d{i:04}"), vec![i as f32, 1.0]));
        }
        pstore_format::index_section_len(&w.finish()).unwrap()
    };
    let mut w = SegmentWriter::new(16).with_index_budget(one_block - 24 + 64);
    for i in 0..300usize {
        let mut d = Document::new(format!("d{i:04}"), vec![i as f32, 1.0]);
        d.attrs.insert(format!("k{i}"), Value::Int(i as i64));
        w.push(d);
    }
    let s = MemoryStore::new();
    let key = Key::new("wide/doubling.seg");
    s.put(&key, w.finish()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(seg.block_count(), 3);
    let k1 = pstore_format::Filter::Gt("k1".to_owned(), 0);
    assert_eq!(
        seg.blocks_to_read(Some(&k1)).len(),
        3,
        "a zone map survived"
    );
    assert_eq!(seg.scan(&s, &key, None).await.unwrap().len(), 300);
}

#[test]
fn every_width_near_the_budget_seals() {
    // ⚠️ Found at code review. The fitting loop measured the block index against the whole
    // budget; the refusal measures the whole meta region -- directory and field table too.
    // An index that fit with less than that overhead to spare passed the loop, skipped the
    // zone-map fallback, and was refused: one document with 174..=176 integer names, or an
    // attribute-free segment of 403..=407 blocks. So every width across both bands is sealed,
    // rather than one fixture that happens to miss the band.
    for names in 150..=200usize {
        let mut d = Document::new("d", vec![1.0, 0.0]);
        for k in 0..names {
            d.attrs
                .insert(format!("score_{k:020}"), Value::Int(k as i64));
        }
        let mut w = SegmentWriter::new(16);
        w.push(d);
        assert!(
            w.try_finish().is_ok(),
            "one document with {names} integer names was refused"
        );
    }
    for rows in (6_400..=6_560usize).step_by(8) {
        let mut w = SegmentWriter::new(16);
        for i in 0..rows {
            w.push(Document::new(format!("d{i}"), vec![i as f32]));
        }
        assert!(
            w.try_finish().is_ok(),
            "{rows} attribute-free rows were refused"
        );
    }
}
