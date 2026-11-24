//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M289: dense fields stored f16 or i8. Every reader of a dense section returns exactly the
//! codec's decoded values, for fixed, named and varying fields (sparse is not stored here); f32 stays version 3 and its
//! bytes; a narrow field makes version 4; row blocks never narrow.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::narrow::{self, Elem, Vectors};
use pstore_format::{
    BlockOptions, DEFAULT_FIELD, Document, Section, Segment, SegmentWriter, VectorField,
};
use std::collections::BTreeMap;

const TAIL: u8 = 1;

/// Deterministic components across many magnitudes, some rows integers.
fn component(i: usize, j: usize) -> f32 {
    let h = (i as u64 * 2_654_435_761 + j as u64 * 40_503) % 10_007;
    let x = h as f32 / 10_007.0 - 0.5;
    match i % 4 {
        0 => (x * 255.0).round(),
        1 => x * 1e-3,
        2 => x * 37.5,
        _ => x,
    }
}

fn vector(i: usize, k: usize, dims: usize) -> Vec<f32> {
    (0..dims).map(|j| component(i + 7 * k, j)).collect()
}

fn docs(n: usize, many: bool) -> Vec<Document> {
    (0..n)
        .map(|i| {
            let mut vectors: BTreeMap<pstore_format::Field, VectorField> = BTreeMap::new();
            vectors.insert(DEFAULT_FIELD.into(), VectorField::dense(vector(i, 0, 9)));
            if many {
                vectors.insert("named".into(), VectorField::dense(vector(i, 1, 4)));
                vectors.insert(
                    "multi".into(),
                    VectorField::Dense((0..i % 3).map(|k| vector(i, k + 2, 3)).collect()),
                );
            }
            Document {
                id: format!("d{i:03}"),
                vectors,
                attrs: BTreeMap::new(),
            }
        })
        .collect()
}

fn options(elem: Elem, columnar: bool) -> BlockOptions {
    BlockOptions {
        columnar,
        vectors: Vectors { elem, tail: TAIL },
        ..BlockOptions::default()
    }
}

fn seal(docs: &[Document], o: BlockOptions) -> bytes::Bytes {
    let mut w = SegmentWriter::new(8).with_block_options(o);
    for d in docs {
        w.push(d.clone());
    }
    w.try_finish().unwrap()
}

/// The footer's version, `MAGIC(8) VERSION(2) ...` a fixed distance from the end.
fn version(seg: &[u8]) -> u16 {
    let at = seg.len() - (8 + 2 + 8 + 4 + 4 + 8 + 8) + 8;
    u16::from_le_bytes([seg[at], seg[at + 1]])
}

async fn open(bytes: bytes::Bytes) -> (MemoryStore, Key, Segment) {
    let s = MemoryStore::new();
    let key = Key::new("seg");
    s.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    (s, key, seg)
}

/// What the codec makes of `v`, stored as `elem`.
fn stored(elem: Elem, v: &[f32]) -> Vec<f32> {
    let tail = usize::from(TAIL).min(v.len());
    let mut raw = Vec::new();
    narrow::encode(elem, v, v.len(), tail, &mut raw);
    narrow::decode(elem, &raw, v.len(), tail).unwrap()
}

fn expect(elem: Elem, d: &Document, field: &str) -> Vec<Vec<f32>> {
    d.field(field).iter().map(|v| stored(elem, v)).collect()
}

#[tokio::test]
async fn every_reader_returns_the_codecs_values_in_every_mode() {
    let docs = docs(60, true);
    for elem in [Elem::F32, Elem::F16, Elem::I8] {
        let bytes = seal(&docs, options(elem, true));
        assert_eq!(version(&bytes), if elem == Elem::F32 { 3 } else { 4 });
        let (s, key, seg) = open(bytes).await;
        for name in [DEFAULT_FIELD, "named", "multi"] {
            let f = seg.field_layout(name).unwrap();
            assert_eq!((f.kind, f.elem), (0, elem), "{name}");
            assert_eq!(f.tail, if elem == Elem::F32 { 0 } else { TAIL }, "{name}");
        }
        // The whole of each field, one read and together.
        let all = seg
            .read_fields(&s, &key, &[DEFAULT_FIELD, "named", "multi"])
            .await
            .unwrap();
        for name in [DEFAULT_FIELD, "named", "multi"] {
            let one = seg.field_vectors(&s, &key, name).await.unwrap();
            assert_eq!(all[name], one, "{elem:?} {name}");
            for (d, got) in docs.iter().zip(&one) {
                assert_eq!(*got, expect(elem, d, name), "{elem:?} {name} {}", d.id);
            }
        }
        // Named rows of a fixed and a varying field, and a varying field's ranges.
        let rows = [0usize, 5, 13, 41, 59];
        for name in [DEFAULT_FIELD, "named", "multi"] {
            let got = seg.field_rows(&s, &key, name, &rows).await.unwrap();
            for r in rows {
                assert_eq!(got[&r], expect(elem, &docs[r], name), "{elem:?} {name} {r}");
            }
        }
        let spans = seg.field_spans(&s, &key, "multi", &rows).await.unwrap();
        let layout = seg.field_layout("multi").unwrap();
        for r in rows {
            let span = &spans[&r];
            let raw = if span.start < span.end {
                s.get_range(&key, span.clone()).await.unwrap().to_vec()
            } else {
                Vec::new()
            };
            assert_eq!(
                layout.decode_vectors(&raw).unwrap(),
                expect(elem, &docs[r], "multi"),
                "{elem:?} {r}"
            );
        }
        // The legacy `Vectors` readers, `vector`'s rows.
        assert!(seg.reads_vector_rows());
        let got = seg.vector_rows(&s, &key, &rows).await.unwrap();
        for r in rows {
            assert_eq!(vec![got[&r].clone()], expect(elem, &docs[r], DEFAULT_FIELD));
        }
        let whole = seg.vectors(&s, &key).await.unwrap();
        for (d, v) in docs.iter().zip(&whole) {
            assert_eq!(vec![v.clone()], expect(elem, d, DEFAULT_FIELD));
        }
        // Every field of every document.
        let scanned = seg.scan(&s, &key, None).await.unwrap();
        assert_eq!(scanned.len(), docs.len());
        for d in &scanned {
            let src = docs.iter().find(|x| x.id == d.id).unwrap();
            for name in [DEFAULT_FIELD, "named", "multi"] {
                assert_eq!(d.field(name), expect(elem, src, name), "{elem:?} {name}");
            }
        }
    }
}

/// `scan`'s inline path: one fixed field, sliced by the blocks read.
#[tokio::test]
async fn a_single_field_scan_decodes_its_inline_slices() {
    let docs = docs(30, false);
    for elem in [Elem::F16, Elem::I8] {
        let (s, key, seg) = open(seal(&docs, options(elem, true))).await;
        for d in seg.scan(&s, &key, None).await.unwrap() {
            let src = docs.iter().find(|x| x.id == d.id).unwrap();
            assert_eq!(d.field(DEFAULT_FIELD), expect(elem, src, DEFAULT_FIELD));
        }
    }
}

#[tokio::test]
async fn f32_is_written_as_before_and_row_blocks_never_narrow() {
    let docs = docs(40, true);
    // f32 under the setting is the writer's default, byte for byte.
    let mut w = SegmentWriter::new(8);
    for d in &docs {
        w.push(d.clone());
    }
    assert_eq!(
        seal(&docs, options(Elem::F32, true)),
        w.try_finish().unwrap()
    );
    for elem in [Elem::F16, Elem::I8] {
        let rows = seal(&docs, options(elem, false));
        assert_eq!(rows, seal(&docs, options(Elem::F32, false)), "{elem:?}");
        assert!(version(&rows) <= 2);
        let (_, _, seg) = open(rows).await;
        assert!(seg.fields().iter().all(|f| f.elem == Elem::F32));
    }
}

#[tokio::test]
async fn a_field_its_type_cannot_hold_stays_f32_in_that_segment() {
    let mut docs = self::docs(20, true);
    // A component that rounds to infinity in f16 (the tail is not judged).
    docs[3].vectors.insert(
        "named".into(),
        VectorField::dense(vec![1.0, 65_520.0, 2.0, 3.0]),
    );
    // And in the varying field: `vector` alone narrow.
    docs[4].vectors.insert(
        "multi".into(),
        VectorField::Dense(vec![vec![70_000.0, 0.0, 0.0]]),
    );
    let bytes = seal(&docs, options(Elem::F16, true));
    assert_eq!(version(&bytes), 4);
    let (s, key, seg) = open(bytes).await;
    assert_eq!(seg.field_layout("named").unwrap().elem, Elem::F32);
    assert_eq!(seg.field_layout("multi").unwrap().elem, Elem::F32);
    assert_eq!(seg.field_layout(DEFAULT_FIELD).unwrap().elem, Elem::F16);
    // The legacy readers decode `vector` by its own layout, not another field's.
    let rows = seg.vector_rows(&s, &key, &[0, 7]).await.unwrap();
    for r in [0usize, 7] {
        assert_eq!(
            vec![rows[&r].clone()],
            expect(Elem::F16, &docs[r], DEFAULT_FIELD)
        );
    }
    let named = seg.field_vectors(&s, &key, "named").await.unwrap();
    assert_eq!(named[3], [vec![1.0, 65_520.0, 2.0, 3.0]]);
    // Alone, no field is narrow, and the segment is version 3.
    let only: Vec<Document> = docs
        .iter()
        .map(|d| {
            let mut d = d.clone();
            d.vectors.retain(|n, _| n.as_str() == "named");
            d
        })
        .collect();
    assert_eq!(version(&seal(&only, options(Elem::F16, true))), 3);
    // i8 likewise past 2^127, where a code would decode to infinity.
    let mut big = only.clone();
    big[5].vectors.insert(
        "named".into(),
        VectorField::dense(vec![f32::MAX, 0.0, 0.0, 0.0]),
    );
    let (s2, k2, seg2) = open(seal(&big, options(Elem::I8, true))).await;
    assert_eq!(seg2.field_layout("named").unwrap().elem, Elem::F32);
    assert_eq!(
        seg2.field_vectors(&s2, &k2, "named").await.unwrap()[5],
        [vec![f32::MAX, 0.0, 0.0, 0.0]]
    );
    assert_eq!(version(&seal(&only, options(Elem::I8, true))), 4);
    // A tail as wide as the vector leaves it f32 too.
    let narrow = SegmentWriter::new(8).with_block_options(BlockOptions {
        vectors: Vectors {
            elem: Elem::I8,
            tail: 9,
        },
        ..BlockOptions::default()
    });
    let mut w = narrow;
    for d in self::docs(10, false) {
        w.push(d);
    }
    assert_eq!(version(&w.try_finish().unwrap()), 3);
}

#[tokio::test]
async fn a_corrupt_exponent_is_refused_not_misread() {
    let docs = docs(10, false);
    let bytes = seal(&docs, options(Elem::I8, true));
    let (_, _, seg) = open(bytes.clone()).await;
    let span = seg.field_section(DEFAULT_FIELD, Section::Vectors).unwrap();
    let mut bad = bytes.to_vec();
    // Row 2's exponent byte: the body's checksum is not the meta region's.
    let at = span.start as usize + 2 * seg.field_layout(DEFAULT_FIELD).unwrap().vector_len();
    bad[at] = 0x80;
    let (s, key, seg) = open(bytes::Bytes::from(bad)).await;
    assert!(seg.field_vectors(&s, &key, DEFAULT_FIELD).await.is_err());
    assert!(seg.vector_rows(&s, &key, &[2]).await.is_err());
    assert!(seg.field_rows(&s, &key, DEFAULT_FIELD, &[2]).await.is_err());
    assert!(seg.scan(&s, &key, None).await.is_err());
    // Row 1 is intact.
    assert_eq!(
        vec![seg.vector_rows(&s, &key, &[1]).await.unwrap()[&1].clone()],
        expect(Elem::I8, &docs[1], DEFAULT_FIELD)
    );
}
