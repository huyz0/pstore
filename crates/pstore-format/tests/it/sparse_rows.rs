//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M193: `Section::SparseRows`, each row's pairs stored forward beside the postings.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::sparse::{DEFAULT_ENCODING, build, decode_pairs, rows_section, stored_weight};
use pstore_format::{Document, Impact, Section, Segment, SegmentWriter, VectorField};

/// Row `i`'s pairs, written out of key order: none for every fifth row, else keys of
/// lengths 1 to 4 with weights whose f16 rounding loses bits, a zero and a negative.
fn pairs(i: usize) -> Vec<(String, Impact)> {
    if i % 5 == 2 {
        return Vec::new();
    }
    (0..1 + i % 4)
        .rev()
        .map(|j| {
            let k = "zyxw"[..=j].to_owned() + &i.to_string();
            let w = match j {
                0 => 0.0,
                1 => -1.0 - i as f32 / 3.0,
                _ => 0.1 + i as f32 / 7.0,
            };
            (k, Impact::new(w))
        })
        .collect()
}

fn docs(n: usize) -> Vec<Document> {
    (0..n)
        .map(|i| {
            let mut d = Document::new(format!("d{i:02}"), vec![i as f32, 1.0]);
            let p = pairs(i);
            if !p.is_empty() {
                d.vectors.insert("terms".into(), VectorField::Sparse(p));
            }
            d.vectors
                .insert("late".into(), VectorField::Dense(vec![vec![1.0; 3]; i % 3]));
            d
        })
        .collect()
}

/// Row `i`'s pairs as the section answers them: in key byte order, weights rounded.
fn stored(i: usize) -> Vec<(String, f32)> {
    let mut p: Vec<(String, f32)> = pairs(i)
        .into_iter()
        .map(|(k, w)| (k, stored_weight(w.get())))
        .collect();
    p.sort_by(|a, b| a.0.cmp(&b.0));
    p
}

fn answered(raw: &[u8]) -> Vec<(String, f32)> {
    decode_pairs(raw)
        .unwrap()
        .into_iter()
        .map(|(k, w)| (k, w.get()))
        .collect()
}

#[test]
fn the_section_is_an_offset_table_then_each_rows_pairs_in_key_order() {
    let docs = docs(12);
    let s = rows_section(&docs, "terms");
    let off = |r: usize| u64::from_le_bytes(s[r * 8..r * 8 + 8].try_into().unwrap()) as usize;
    let table = 8 * (docs.len() + 1);
    assert_eq!(off(0), 0);
    assert_eq!(
        table + off(docs.len()),
        s.len(),
        "the last offset ends the section"
    );
    for i in 0..docs.len() {
        let raw = &s[table + off(i)..table + off(i + 1)];
        // A pair is a u16 key length, the key and the stored F16's two bytes.
        let width: usize = pairs(i).iter().map(|(k, _)| k.len() + 4).sum();
        assert_eq!(raw.len(), width, "row {i}");
        assert_eq!(answered(raw), stored(i), "row {i}");
        if let Some((k, _)) = stored(i).first() {
            assert_eq!(usize::from(u16::from_le_bytes([raw[0], raw[1]])), k.len());
            assert_eq!(
                &raw[2..2 + k.len()],
                k.as_bytes(),
                "row {i}: the least key first"
            );
        }
    }
    // Weights are rounded at f16, and the signed zero and the negatives survive.
    assert_ne!(stored_weight(0.1 + 1.0 / 7.0), 0.1 + 1.0 / 7.0);
    assert!(stored_weight(-0.0).is_sign_negative());
    assert_eq!(stored_weight(-2.0), -2.0);
    // A field no row holds is a table of zeros.
    assert_eq!(rows_section(&docs, "nope"), vec![0u8; table]);
}

#[test]
fn decode_pairs_refuses_bytes_that_are_not_whole_pairs() {
    let docs = docs(4);
    let s = rows_section(&docs, "terms");
    let table = 8 * 5;
    // Row 3 holds 4 pairs.
    let row = &s[table..];
    let row = &row[row.len() - pairs(3).iter().map(|(k, _)| k.len() + 4).sum::<usize>()..];
    assert_eq!(answered(row), stored(3));
    for cut in 1..row.len() {
        let got = decode_pairs(&row[..cut]);
        // A prefix decodes only where it ends a pair.
        let whole: Vec<usize> = stored(3)
            .iter()
            .scan(0, |at, (k, _)| {
                *at += k.len() + 4;
                Some(*at)
            })
            .collect();
        assert_eq!(got.is_some(), whole.contains(&cut), "cut at {cut}");
    }
    assert_eq!(decode_pairs(&[]).unwrap(), Vec::new());
    // A key that is not UTF-8.
    assert!(decode_pairs(&[1, 0, 0xff, 0, 0]).is_none());
}

#[tokio::test]
async fn sparse_spans_locate_each_rows_pairs_and_say_when_the_section_is_absent() {
    let store = MemoryStore::new();
    let docs = docs(30);
    let p = build(&docs, "terms", DEFAULT_ENCODING);
    let mut with = SegmentWriter::new(2);
    let mut without = SegmentWriter::new(2);
    let mut plain = SegmentWriter::new(2);
    for d in &docs {
        with.push(d.clone());
        without.push(d.clone());
        let mut d = d.clone();
        d.vectors.remove("terms");
        plain.push(d);
    }
    let put = async |name: &str, bytes: bytes::Bytes| {
        let key = Key::new(name);
        store.put(&key, bytes).await.unwrap();
        (Segment::open(&store, &key).await.unwrap(), key)
    };
    let (seg, key) = put(
        "t/i/with.seg",
        with.with_section(Section::SparsePostings, p.section.clone())
            .with_section(Section::SparseRows, p.rows.clone())
            .try_finish()
            .unwrap(),
    )
    .await;
    let rows = [0usize, 2, 7, 29, 7, 30, 1000];
    let spans = seg
        .sparse_spans(&store, &key, &rows)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        spans.keys().copied().collect::<Vec<_>>(),
        [0, 2, 7, 29],
        "a duplicate and rows at and past the end are left out"
    );
    for (row, span) in &spans {
        let raw = store.get_range(&key, span.clone()).await.unwrap();
        assert_eq!(answered(&raw), stored(*row), "row {row}");
    }
    assert!(
        spans[&2].is_empty() && spans[&7].is_empty(),
        "rows holding none"
    );
    assert!(
        seg.sparse_spans(&store, &key, &[])
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );

    // A sparse field and no section: folded before M193.
    let (seg, key) = put(
        "t/i/without.seg",
        without
            .with_section(Section::SparsePostings, p.section.clone())
            .try_finish()
            .unwrap(),
    )
    .await;
    assert!(
        seg.sparse_spans(&store, &key, &rows)
            .await
            .unwrap()
            .is_none()
    );

    // No sparse field: nothing to answer, and nothing refused.
    let (seg, key) = put("t/i/plain.seg", plain.try_finish().unwrap()).await;
    assert!(
        seg.sparse_spans(&store, &key, &rows)
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
}
