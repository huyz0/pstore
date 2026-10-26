//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Criterion 4 end-to-end: a rung-0 scan reads codes and nothing else.
//!
//! `pstore-format` cannot test this with real codes — it is layer 2 and the quantizer is
//! layer 3, so its own section tests use opaque bytes. Here the dependency runs the right
//! way and the codes are the ones the system will actually store, which is the difference
//! between testing the layout and testing the claim.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, Segment, SegmentWriter};
use pstore_index::{rabitq::Quantizer, sq8};
use pstore_types::TenantId;

const DIM: usize = 384;
const N: usize = 2_000;

fn corpus() -> Vec<Document> {
    let mut state = 12345u64;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((state >> 33) as f32 / (1u64 << 31) as f32) - 0.5
    };
    (0..N)
        .map(|i| {
            let mut v: Vec<f32> = (0..DIM).map(|_| next()).collect();
            let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            for x in v.iter_mut() {
                *x /= n;
            }
            Document::new(format!("d{i}"), v)
        })
        .collect()
}

/// Builds the segment the way the engine will: quantize, then hand the bytes to the format.
fn build(docs: &[Document]) -> bytes::Bytes {
    let q = Quantizer::new(DIM);
    let mut rabitq = Vec::new();
    let mut eights = Vec::new();
    for d in docs {
        q.encode(d.vector()).unwrap().write_to(&mut rabitq);
        sq8::write_to(&sq8::encode(d.vector()), &mut eights);
    }
    let mut w = SegmentWriter::new(64);
    for d in docs {
        w.push(d.clone());
    }
    w.with_section(Section::RaBitQ, rabitq)
        .with_section(Section::Sq8, eights)
        .finish()
}

#[tokio::test]
async fn a_rung_zero_scan_reads_no_full_precision_or_int8_bytes() {
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let v = s.as_tenant(t);
    let key = Key::new("seg/rung0");
    let docs = corpus();
    v.put(&key, build(&docs)).await.unwrap();
    let seg = Segment::open(&v, &key).await.unwrap();

    let vectors = seg.section(Section::Vectors).unwrap();
    let eights = seg.section(Section::Sq8).unwrap();
    s.record_ranges();
    let codes = seg
        .fetch_section(&v, &key, Section::RaBitQ)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        s.bytes_in(&key, vectors.clone()),
        0,
        "rung 0 touched the full-precision section"
    );
    assert_eq!(
        s.bytes_in(&key, eights.clone()),
        0,
        "rung 0 touched the int8 section, which only rung 1 may read"
    );
    assert!(s.bytes_in(&key, seg.section(Section::RaBitQ).unwrap()) > 0);

    // ⚠️ And the codes actually work. Reading the right bytes cheaply is worth nothing if
    // they do not answer the query -- a section fetch that returned zeros would satisfy
    // every byte assertion above.
    let q = Quantizer::new(DIM);
    let width = codes.len() / N;
    let query = docs[7].vector();
    let prepared = q.prepare(query).unwrap();
    let mut best = (0usize, f32::NEG_INFINITY);
    for i in 0..N {
        let code = q
            .read_code(codes.get(i * width..(i + 1) * width).unwrap())
            .unwrap();
        let score = q.estimate_prepared(&code, &prepared);
        if score > best.1 {
            best = (i, score);
        }
    }
    assert_eq!(best.0, 7, "the nearest vector to itself was not itself");
}

#[tokio::test]
async fn the_code_sections_are_a_fraction_of_the_vectors() {
    // The compression the round-trip budget is built on, as measured bytes.
    let s = MemoryStore::new();
    let key = Key::new("seg/ratio");
    s.put(&key, build(&corpus())).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let len = |x| {
        let r: std::ops::Range<u64> = seg.section(x).unwrap();
        r.end - r.start
    };
    let (vectors, rabitq, eights) = (
        len(Section::Vectors),
        len(Section::RaBitQ),
        len(Section::Sq8),
    );
    assert_eq!(vectors, (N * DIM * 4) as u64);
    // 384 dims pad to 512 bits = 64 bytes, plus 4 for the alignment and 4 for the residual
    // norm. Both are per-vector scalars the estimate cannot work without: the alignment
    // de-biases, and the norm restores the scale the residual was divided by.
    assert_eq!(rabitq, (N * 72) as u64);
    // One byte a dimension, plus the per-vector offset and step the reader needs to put
    // two rows' codes on the same scale.
    assert_eq!(eights, (N * (DIM + 8)) as u64);
    // 21x, not the 24x the bit count alone suggests: eight bytes a vector of scalars buy
    // the error bound and the residual scale. Stated as the measured ratio rather than the
    // theoretical one, because the theoretical one is not what gets fetched.
    assert!(
        vectors / rabitq >= 21,
        "1-bit compression is only {}x",
        vectors / rabitq
    );
    assert!(
        vectors / eights == 3,
        "int8 is {}x smaller; the scale costs eight bytes a row",
        vectors / eights
    );
}
