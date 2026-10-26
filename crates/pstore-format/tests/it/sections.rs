//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Segment sections, and the one property they exist for.
//!
//! A rung-0 scan reads 1-bit codes: 48 bytes a vector at 384 dimensions. The full-precision
//! vectors are 1,536 bytes a vector — **32× more**. If those live in the same bytes as the
//! codes, every approximate query pays for precision it discards, and the compression that
//! makes the whole round-trip argument work is spent before it is used.
//!
//! So the separation is not tidiness. It is the difference between fetching 3 MB and
//! fetching 98 MB for the same answer, and it is asserted with the byte counter rather
//! than by reading the layout.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, Segment, SegmentWriter, Value};
use pstore_types::TenantId;

const DIM: usize = 384;

fn doc(i: usize) -> Document {
    let mut d = Document::new(
        format!("d{i}"),
        (0..DIM).map(|j| ((i + j) % 17) as f32 * 0.1).collect(),
    );
    d.attrs.insert("n".to_owned(), Value::Int(i as i64));
    d
}

/// A segment carrying stand-in code sections.
///
/// ⚠️ The codes are opaque bytes here on purpose. `pstore-format` is layer 2 and the
/// quantizer is layer 3, so a writer that called it would invert the dependency and put the
/// architecture in a comment instead of in `Cargo.toml`. The format owns *where sections
/// live*; whoever owns the codes hands them over. The end-to-end assertion with real
/// RaBitQ codes lives in `pstore-index`, where the dependency runs the right way.
fn segment(n: usize) -> bytes::Bytes {
    let mut w = SegmentWriter::new(64);
    for i in 0..n {
        w.push(doc(i));
    }
    // 64 bytes a row: what a padded 384-dimension 1-bit code actually costs.
    let rabitq = vec![0xA5u8; n * 64];
    // 384 bytes a row: int8, one per dimension.
    let sq8 = vec![0x5Au8; n * DIM];
    w.with_section(Section::RaBitQ, rabitq)
        .with_section(Section::Sq8, sq8)
        .finish()
}

#[tokio::test]
async fn a_segment_names_its_sections_in_the_footer() {
    // Readers skip what they do not find, which is what lets a v1 segment stay valid
    // forever once sparse postings and positions exist. The directory is the mechanism.
    let s = MemoryStore::new();
    let key = Key::new("seg/sections");
    s.put(&key, segment(500)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();

    for want in [
        Section::Blocks,
        Section::Vectors,
        Section::RaBitQ,
        Section::Sq8,
    ] {
        let span = seg.section(want);
        assert!(span.is_some(), "{want:?} missing from the directory");
    }
    // Reserved but not written: a reader must report absence, not guess at an offset.
    assert!(seg.section(Section::SparsePostings).is_none());
}

#[tokio::test]
async fn sections_do_not_overlap() {
    // ⚠️ Overlap is the failure that looks like success: every section reads back
    // plausible-looking bytes, and only the numbers are wrong. It was the M1 data-leak bug
    // one layer down -- a `SegmentRef` pointing at the whole object rather than its slice.
    let s = MemoryStore::new();
    let key = Key::new("seg/overlap");
    s.put(&key, segment(300)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();

    let mut spans: Vec<_> = [
        Section::Blocks,
        Section::Vectors,
        Section::RaBitQ,
        Section::Sq8,
    ]
    .into_iter()
    .filter_map(|x| seg.section(x))
    .collect();
    spans.sort_by_key(|r| r.start);
    for pair in spans.windows(2) {
        assert!(
            pair[0].end <= pair[1].start,
            "sections overlap: {:?} and {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[tokio::test]
async fn a_quantised_scan_reads_no_full_precision_bytes() {
    // ⚠️ Criterion 4 of the M3 spec, and the reason sections exist at all.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let v = s.as_tenant(t);
    let key = Key::new("seg/rung0");
    v.put(&key, segment(2_000)).await.unwrap();
    let seg = Segment::open(&v, &key).await.unwrap();

    let vectors = seg.section(Section::Vectors).unwrap();
    s.record_ranges();
    let codes = seg
        .fetch_section(&v, &key, Section::RaBitQ)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(codes.len(), 2_000 * 64);

    assert_eq!(
        s.bytes_in(&key, vectors.clone()),
        0,
        "a rung-0 scan touched the full-precision section"
    );
    // And it really did read the codes, or the assertion above is satisfied by reading
    // nothing at all.
    let rabitq = seg.section(Section::RaBitQ).unwrap();
    assert!(s.bytes_in(&key, rabitq.clone()) > 0);
}

#[tokio::test]
async fn one_bit_codes_are_a_fraction_of_the_vectors_they_stand_in_for() {
    // The compression claim, as a number rather than an adjective. At 384 dimensions a
    // float32 vector is 1,536 bytes and its padded 1-bit code is 64: 24x. If this ratio
    // ever drops, the round-trip budget stops closing and nothing else says so.
    let s = MemoryStore::new();
    let key = Key::new("seg/ratio");
    s.put(&key, segment(1_000)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let vectors = seg.section(Section::Vectors).unwrap();
    let rabitq = seg.section(Section::RaBitQ).unwrap();
    let ratio = (vectors.end - vectors.start) / (rabitq.end - rabitq.start);
    assert!(
        ratio >= 20,
        "1-bit codes are only {ratio}x smaller than float32"
    );
}

#[tokio::test]
async fn a_scan_still_returns_whole_documents() {
    // Moving vectors out of the blocks must not change what a caller sees. This is the
    // regression that would otherwise show up as an empty vector three layers away.
    let s = MemoryStore::new();
    let key = Key::new("seg/whole");
    s.put(&key, segment(200)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let all = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(all.len(), 200);
    assert_eq!(all[7].id, "d7");
    assert_eq!(all[7].vector(), doc(7).vector());
    assert_eq!(all[7].attrs.get("n"), Some(&Value::Int(7)));
}

#[tokio::test]
async fn a_segment_with_no_vectors_omits_the_vector_sections() {
    // Attribute-only rows are legal, and a section written empty would cost a directory
    // entry and an offset for nothing.
    let s = MemoryStore::new();
    let key = Key::new("seg/novec");
    let mut w = SegmentWriter::new(8);
    for i in 0..10 {
        w.push(Document::new(format!("d{i}"), Vec::new()));
    }
    s.put(&key, w.finish()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert!(seg.section(Section::Vectors).is_none());
    assert!(seg.section(Section::RaBitQ).is_none());
    assert_eq!(seg.scan(&s, &key, None).await.unwrap().len(), 10);
}

#[tokio::test]
async fn a_segment_from_the_future_is_readable_not_refused() {
    // ⚠️ Forward compatibility, and it is not decoration. Segments are IMMUTABLE, so
    // "migrate the old ones" is never available and a reader written today will meet
    // segments written by every later version for as long as the data lives. A reader that
    // refused an unrecognised section id would make every new section a breaking change for
    // every deployed node at once.
    let s = MemoryStore::new();
    let key = Key::new("seg/future");
    let mut w = SegmentWriter::new(16);
    for i in 0..100 {
        w.push(doc(i));
    }
    let bytes = w
        .with_section(Section::RaBitQ, vec![1u8; 100 * 64])
        .with_raw_section(9_999, vec![0xEEu8; 4_096])
        .finish();
    s.put(&key, bytes).await.unwrap();

    // Opens, and everything this version does understand still works.
    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(seg.scan(&s, &key, None).await.unwrap().len(), 100);
    assert!(seg.section(Section::RaBitQ).is_some());
    assert!(seg.section(Section::Vectors).is_some());
}

#[tokio::test]
async fn the_vector_section_starts_after_the_last_data_block() {
    // ⚠️ The data blocks are addressed by the index, not by the directory, so an overlap
    // between them and a section is invisible to `sections_do_not_overlap` -- the mutation
    // that writes vectors at offset 0 passes it. This is the assertion that sees it.
    let s = MemoryStore::new();
    let key = Key::new("seg/after");
    s.put(&key, segment(400)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let end = seg.data_end();
    let vectors = seg.section(Section::Vectors).unwrap();
    assert!(
        vectors.start >= end,
        "the vectors section starts at {} but the data blocks run to {end}",
        vectors.start
    );
}

/// A document with an attribute and no vector field, so the body is its data blocks alone.
fn plain(i: usize) -> Document {
    let mut d = Document {
        id: format!("p{i}"),
        vectors: Default::default(),
        attrs: Default::default(),
    };
    d.attrs.insert("n".to_owned(), Value::Int(i as i64));
    d
}

#[tokio::test]
async fn a_segment_is_its_data_blocks_its_index_and_its_footer() {
    // Nothing between them and nothing after: the index is the meta region the footer
    // points at, and the footer is what the suffix read has left over from the budget.
    let footer = pstore_format::SUFFIX_FETCH as usize - pstore_format::INDEX_BUDGET;
    for n in [1usize, 50, 600] {
        let mut w = SegmentWriter::new(16);
        for i in 0..n {
            w.push(plain(i));
        }
        let bytes = w.finish();
        let s = MemoryStore::new();
        let key = Key::new("seg/exact");
        s.put(&key, bytes.clone()).await.unwrap();
        let seg = Segment::open(&s, &key).await.unwrap();
        let index = pstore_format::index_section_len(&bytes).unwrap();
        assert_eq!(
            seg.data_end() as usize + index + footer,
            bytes.len(),
            "{n} rows: blocks end at {}, the index is {index} bytes",
            seg.data_end()
        );
    }
}

#[tokio::test]
async fn a_vectors_section_with_no_rows_reads_as_no_vectors() {
    // `with_section` is public, so a segment can carry a `Vectors` section and no rows.
    // Its row width is 0, not a division by zero, and asking for a row fetches nothing.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(7);
    let ts = s.as_tenant(t);
    let key = Key::new("seg/empty-vectors");
    let bytes = SegmentWriter::new(16)
        .with_section(Section::Vectors, vec![0u8; 16])
        .finish();
    ts.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&ts, &key).await.unwrap();
    let before = s.count(t, pstore_blob::OpClass::Read);
    assert!(seg.vector_rows(&ts, &key, &[0]).await.unwrap().is_empty());
    assert!(seg.scan(&ts, &key, None).await.unwrap().is_empty());
    assert_eq!(s.count(t, pstore_blob::OpClass::Read) - before, 0);
}
