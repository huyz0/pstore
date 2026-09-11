//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Two row spaces in one segment — M3c.1.
//!
//! ⚠️ **The sharpest edge in the format.** A boundary vector belongs to two posting lists, so
//! its *codes* appear twice; the *document* must appear once. That makes the code sections
//! longer than the block index, and every fixed-width section has to agree about which count
//! it strides by. A stride taken from the wrong one reads every row after the first at an
//! offset and **decodes without complaint** — vectors of plausible noise.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, Segment, SegmentWriter};

const DIM: usize = 4;

fn doc(i: usize) -> Document {
    Document::new(
        format!("d{i:03}"),
        (0..DIM).map(|j| (i * 10 + j) as f32).collect(),
    )
}

/// A segment whose codes cover `index_rows` entries while its blocks hold `docs`.
async fn build(docs: &[Document], index_rows: Option<&[u32]>) -> (MemoryStore, Key, Segment) {
    let s = MemoryStore::new();
    let key = Key::new("seg");
    let mut w = SegmentWriter::new(4);
    for d in docs {
        w.push(d.clone());
    }
    // ⚠️ Full-precision vectors, one per **DATA** row. `Vectors` is the section that must NOT
    // duplicate: 1,536 bytes a row at 384d against 64 for the 1-bit code, so duplicating it
    // spends exactly the storage replication is meant to save. The 1-bit and int8 codes are
    // the ones that stride by the index row count.
    let mut vectors = Vec::new();
    for d in docs {
        for v in d.vector() {
            vectors.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut w = w.with_section(Section::Vectors, vectors);
    if let Some(m) = index_rows {
        w = w.with_index_rows(m);
    }
    s.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    (s, key, seg)
}

#[tokio::test]
async fn every_row_decodes_to_its_own_vector() {
    // ⚠️ Ten documents, fourteen index rows: rows 2, 5, 7 and 9 are replicated. Asserted
    // against the INPUT vectors, never against the segment's own other reads — a stride taken
    // from `row_count()` is self-consistent and wrong.
    let docs: Vec<Document> = (0..10).map(doc).collect();
    let order: Vec<u32> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 2, 5, 7, 9];
    let (s, key, seg) = build(&docs, Some(&order)).await;

    assert_eq!(
        seg.row_count(),
        10,
        "the blocks hold more than the documents"
    );
    assert_eq!(
        seg.index_row_count(),
        14,
        "the codes do not cover the replicas"
    );

    // ⚠️ `Vectors` strides by DATA rows, so every data row must decode to its own document —
    // asserted against the INPUT vectors, never against the segment's own other reads, since
    // a stride taken from the wrong count is self-consistent and wrong.
    let want: Vec<usize> = (0..docs.len()).collect();
    let got = seg.vector_rows(&s, &key, &want).await.unwrap();
    for (row, d) in docs.iter().enumerate() {
        assert_eq!(
            got.get(&row).map(Vec::as_slice),
            Some(d.vector()),
            "data row {row} decoded as something other than its own document"
        );
    }
}

#[tokio::test]
async fn the_mapping_reads_back() {
    let docs: Vec<Document> = (0..6).map(doc).collect();
    let order: Vec<u32> = vec![0, 1, 2, 3, 4, 5, 1, 4];
    let (s, key, seg) = build(&docs, Some(&order)).await;
    assert_eq!(seg.index_rows(&s, &key).await.unwrap(), order);
}

#[tokio::test]
async fn an_unreplicated_segment_is_byte_identical() {
    // ⚠️ Criterion 6: this milestone makes replication correct and turns nothing on. A segment
    // built without it must be the bytes that were already being written, or every existing
    // segment's bytes change for a feature nobody enabled.
    let docs: Vec<Document> = (0..10).map(doc).collect();
    let s = MemoryStore::new();
    let mut w = SegmentWriter::new(4);
    for d in &docs {
        w.push(d.clone());
    }
    let mut vectors = Vec::new();
    for d in &docs {
        for v in d.vector() {
            vectors.extend_from_slice(&v.to_le_bytes());
        }
    }
    let plain = w
        .with_section(Section::Vectors, vectors.clone())
        .try_finish()
        .unwrap();

    // The same writer, told the identity mapping, must decide it has nothing to record.
    let mut w = SegmentWriter::new(4);
    for d in &docs {
        w.push(d.clone());
    }
    let identity: Vec<u32> = (0..docs.len() as u32).collect();
    let with_map = w
        .with_section(Section::Vectors, vectors)
        .with_index_rows(&identity)
        .try_finish()
        .unwrap();
    assert_eq!(
        plain, with_map,
        "an identity mapping was written as a section, changing the bytes of every segment \
         that does not replicate"
    );

    let key = Key::new("seg");
    s.put(&key, plain).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert!(seg.section(Section::IndexRows).is_none());
    assert_eq!(seg.index_row_count(), seg.row_count());
    assert_eq!(seg.index_rows(&s, &key).await.unwrap(), identity);
}
