//! M141: a row's footprint, the estimate a tenant's buffered bytes are bounded by.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_format::{Document, Field, Value, VectorField};

#[test]
fn every_value_kind_is_counted() {
    for (v, want) in [
        (Value::Int(-1), 8),
        (Value::Uint(u64::MAX), 8),
        (Value::Float(1.5), 8),
        (Value::Bool(true), 8),
        (Value::DateTime(0), 8),
        (Value::Str("héllo".to_owned()), 6),
        (
            Value::Array(vec![Value::Int(1), Value::Str("ab".to_owned())]),
            10,
        ),
        (Value::Array(vec![]), 0),
        (Value::Uuid([7; 16]), 16),
        (Value::Bytes(vec![1, 2, 3]), 3),
        (Value::Bytes(vec![]), 0),
        // M175: a null element, its tag.
        (Value::Array(vec![Value::Null, Value::Int(1)]), 9),
    ] {
        assert_eq!(v.footprint(), want, "{v:?}");
    }
}

/// The 16 bytes M213 charges each heap allocation beyond its own.
const A: u64 = 16;

fn heap(cap: usize, each: usize) -> u64 {
    if cap == 0 { 0 } else { (cap * each) as u64 + A }
}

/// One B-tree leaf of 11 entries, its parent pointer and two `u16`s padded to 16 bytes.
fn leaf<K, V>() -> u64 {
    (16 + 11 * (std::mem::size_of::<K>() + std::mem::size_of::<V>())) as u64 + A
}

/// M213: a document is charged the heap it holds and its slot, from capacities and sizes.
#[test]
fn a_document_is_charged_the_memory_it_holds() {
    use std::mem::size_of;
    let slot = 2 * size_of::<Document>() as u64;
    let mut v = Vec::with_capacity(8);
    v.extend([0.0_f32; 5]);
    let mut d = Document::new(String::from("abc"), v);
    let field = |name: &str| 16 + name.len() as u64 + A;
    let dense = |caps: &[usize]| {
        heap(caps.len(), size_of::<Vec<f32>>()) + caps.iter().map(|c| heap(*c, 4)).sum::<u64>()
    };
    let vectors = leaf::<Field, VectorField>();
    let base = slot + heap(3, 1) + vectors + field("vector") + dense(&[8]);
    // Capacity, not length: a vector grown to 8 holds 8.
    assert_eq!(d.footprint(), base);
    d.attrs.insert(
        "k".to_owned(),
        Value::Str(String::with_capacity(10) + "vvvv"),
    );
    let attrs = leaf::<String, Value>();
    assert_eq!(d.footprint(), base + attrs + heap(1, 1) + heap(10, 1));
    d.attrs.insert(
        "arr".to_owned(),
        Value::Array(vec![
            Value::Int(1),
            Value::Bytes(vec![1, 2, 3]),
            Value::Array(vec![Value::Null]),
        ]),
    );
    let arr = heap(3, 1) + heap(3, size_of::<Value>()) + heap(3, 1) + heap(1, size_of::<Value>());
    let with_attrs = base + attrs + heap(1, 1) + heap(10, 1) + arr;
    assert_eq!(d.footprint(), with_attrs);
    d.vectors.insert(
        Field::from("multi"),
        VectorField::Dense(vec![vec![0.0; 2], vec![0.0; 3]]),
    );
    let multi = field("multi") + dense(&[2, 3]);
    assert_eq!(d.footprint(), with_attrs + multi);
    d.vectors.insert(
        Field::from("sp"),
        VectorField::Sparse(vec![
            ("1".to_owned(), pstore_format::Impact::new(1.0)),
            ("abc".to_owned(), pstore_format::Impact::new(2.0)),
        ]),
    );
    let sparse = field("sp")
        + heap(2, size_of::<(String, pstore_format::Impact)>())
        + heap(1, 1)
        + heap(3, 1);
    assert_eq!(d.footprint(), with_attrs + multi + sparse);
    // Past 11 entries a map holds more than one node: never less than one leaf a 5 entries.
    for k in 0..30 {
        d.attrs.insert(format!("n{k:02}"), Value::Int(k));
    }
    let leaves = 32_u64.div_ceil(5);
    assert!(d.footprint() > with_attrs + multi + sparse + (leaves - 1) * attrs);
}
