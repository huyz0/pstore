//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Float and bool values in a segment (M9h.1).

use pstore_format::{Document, SegmentWriter, Value};

/// FNV-1a, 64-bit: a hash stable across Rust versions, unlike `DefaultHasher`.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[test]
fn a_segment_without_floats_keeps_its_bytes() {
    // Recorded on f17fdd7, the commit before M9h.1: a segment holding no float or bool must not move
    // by a byte, or every reader before M9h.1 loses segments it could read.
    let mut w = SegmentWriter::new(4);
    for i in 0..10i64 {
        let mut d = Document::new(format!("d{i}"), vec![i as f32, 1.0, -0.5]);
        d.attrs.insert("n".to_owned(), Value::Int(i * 7 - 20));
        d.attrs
            .insert("tag".to_owned(), Value::Str(format!("t{}", i % 3)));
        w.push(d);
    }
    let bytes = w.finish();
    assert_eq!(
        fnv(&bytes),
        16_603_700_730_368_196_973,
        "len {}",
        bytes.len()
    );
}

fn typed_segment() -> bytes::Bytes {
    let mut w = SegmentWriter::new(2);
    for (i, v) in [
        Value::Float(1.5),
        Value::Bool(true),
        Value::Float(-0.0),
        Value::Bool(false),
        Value::Int(3),
    ]
    .into_iter()
    .enumerate()
    {
        let mut d = Document::new(format!("d{i}"), vec![1.0, 0.0]);
        d.attrs.insert("v".to_owned(), v);
        w.push(d);
    }
    w.finish()
}

/// The footer's version: `MAGIC(8) VERSION(2) ...` at a fixed distance from the end.
fn version(seg: &[u8]) -> u16 {
    let at = seg.len() - (8 + 2 + 8 + 4 + 4 + 8 + 8) + 8;
    u16::from_le_bytes([seg[at], seg[at + 1]])
}

#[tokio::test]
async fn floats_and_bools_round_trip_in_a_version_2_segment() {
    use pstore_blob::{BlobStore, Key, MemoryStore};
    let bytes = typed_segment();
    // Version 2, so a reader from before M9h.1 refuses it at open (spec review, M1).
    assert_eq!(version(&bytes), 2);
    let store = MemoryStore::new();
    let key = Key::new("seg");
    store.put(&key, bytes).await.unwrap();
    let seg = pstore_format::Segment::open(&store, &key).await.unwrap();
    let got: Vec<Value> = seg
        .scan(&store, &key, None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.attrs["v"].clone())
        .collect();
    let want = [
        Value::Float(1.5),
        Value::Bool(true),
        Value::Float(-0.0),
        Value::Bool(false),
        Value::Int(3),
    ];
    assert_eq!(got, want);
    // Structurally, `-0.0` is its own value: it came back as written.
    assert!(matches!(got[2], Value::Float(f) if f.is_sign_negative()));
}

#[test]
fn an_untyped_segment_stays_version_1() {
    let mut w = SegmentWriter::new(2);
    let mut d = Document::new("a", vec![1.0]);
    d.attrs.insert("n".to_owned(), Value::Int(1));
    w.push(d);
    assert_eq!(version(&w.finish()), 1);
}

#[test]
fn a_float_that_is_not_finite_is_refused_where_documents_enter() {
    for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut d = Document::new("a", vec![1.0]);
        d.attrs.insert("x".to_owned(), Value::Float(f));
        assert!(
            pstore_format::check_storable(&d).is_err(),
            "{f} was storable"
        );
    }
    let mut d = Document::new("a", vec![1.0]);
    d.attrs.insert("x".to_owned(), Value::Float(f64::MAX));
    assert!(pstore_format::check_storable(&d).is_ok());
}

#[test]
fn structural_equality_keeps_types_apart() {
    // `Value`'s own order is structural: a filter's numeric line is `cmp_numbers`.
    assert_ne!(Value::Int(2), Value::Float(2.0));
    assert_ne!(Value::Float(0.0), Value::Float(-0.0));
    assert!(Value::Int(i64::MAX) < Value::Str(String::new()));
    assert!(Value::Str("z".to_owned()) < Value::Float(f64::MIN));
    assert!(Value::Float(f64::MAX) < Value::Bool(false));
    assert!(Value::Bool(false) < Value::Bool(true));
    // Arrays compare element by element, not as one equal lump (M9h.2).
    let arr = |n| Value::Array(vec![Value::Int(n)]);
    assert!(arr(1) < arr(2) && arr(1) != arr(2) && arr(2) == arr(2));
    assert!(Value::Bool(true) < Value::Array(vec![]));
}

#[test]
fn numbers_compare_exactly_across_int_and_float() {
    use pstore_format::{Number, cmp_numbers};
    use std::cmp::Ordering::{Equal, Greater, Less};
    let c = |a, b| cmp_numbers(a, b);
    let (i, f) = (Number::Int, Number::Float);
    assert_eq!(
        c(i(9_007_199_254_740_993), f(9_007_199_254_740_992.0)),
        Some(Greater)
    );
    assert_eq!(
        c(f(9_007_199_254_740_992.0), i(9_007_199_254_740_993)),
        Some(Less)
    );
    assert_eq!(c(i(2), f(2.0)), Some(Equal));
    assert_eq!(c(i(2), f(2.5)), Some(Less));
    assert_eq!(c(i(-2), f(-2.5)), Some(Greater));
    assert_eq!(c(i(-3), f(-2.5)), Some(Less));
    assert_eq!(c(i(0), f(-0.0)), Some(Equal));
    assert_eq!(c(f(0.0), f(-0.0)), Some(Equal));
    assert_eq!(c(i(i64::MAX), f(9_223_372_036_854_775_808.0)), Some(Less));
    assert_eq!(c(i(i64::MIN), f(-9_223_372_036_854_775_808.0)), Some(Equal));
    assert_eq!(c(i(i64::MIN), f(-1e19)), Some(Greater));
    assert_eq!(c(i(1), f(f64::NAN)), None);
    assert_eq!(c(i(1), i(2)), Some(Less));
}

#[test]
fn a_segment_holding_only_arrays_is_version_2() {
    // Spec review of M9h.2, m1: typed by the array itself, not by what is inside it.
    for items in [vec![], vec![Value::Int(1), Value::Str("a".to_owned())]] {
        let mut w = SegmentWriter::new(2);
        let mut d = Document::new("a", vec![1.0]);
        d.attrs.insert("n".to_owned(), Value::Array(items.clone()));
        w.push(d);
        assert_eq!(version(&w.finish()), 2, "{items:?}");
    }
}

#[test]
fn an_array_holds_only_finite_scalars() {
    for items in [
        vec![Value::Array(vec![])],
        vec![Value::Int(1), Value::Float(f64::NAN)],
    ] {
        let mut d = Document::new("a", vec![1.0]);
        d.attrs.insert("n".to_owned(), Value::Array(items.clone()));
        assert!(pstore_format::check_storable(&d).is_err(), "{items:?}");
    }
}

#[test]
fn a_nested_array_does_not_decode() {
    // Written past `check_storable`, a nested array must still not come back as data.
    let mut d = Document::new("a", vec![1.0]);
    d.attrs.insert(
        "n".to_owned(),
        Value::Array(vec![Value::Array(vec![Value::Int(1)])]),
    );
    let rows = pstore_format::encode_rows(std::slice::from_ref(&d));
    assert_eq!(
        pstore_format::decode_rows(&rows).err(),
        Some(pstore_format::FormatError::Corrupt(
            "an array inside an array"
        ))
    );
    let mut ok = Document::new("a", vec![1.0]);
    ok.attrs.insert(
        "n".to_owned(),
        Value::Array(vec![Value::Int(1), Value::Bool(true)]),
    );
    let rows = pstore_format::encode_rows(std::slice::from_ref(&ok));
    assert_eq!(
        pstore_format::decode_rows(&rows).unwrap()[0].attrs,
        ok.attrs
    );
}
