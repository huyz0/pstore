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
    ] {
        assert_eq!(v.footprint(), want, "{v:?}");
    }
}

#[test]
fn a_document_counts_its_id_vectors_and_attributes() {
    let mut d = Document::new("abc", vec![0.0; 5]);
    // A vector field's name is not counted, whatever it is.
    assert_eq!(d.footprint(), 3 + 20);
    d.attrs
        .insert("k".to_owned(), Value::Str("vvvv".to_owned()));
    assert_eq!(d.footprint(), 3 + 20 + 1 + 4);
    d.vectors.insert(
        Field::from("multi"),
        VectorField::Dense(vec![vec![0.0; 2], vec![0.0; 3]]),
    );
    assert_eq!(d.footprint(), 3 + 20 + 5 + 20);
    d.vectors.insert(
        Field::from("sp"),
        VectorField::Sparse(vec![
            (1, pstore_format::Impact::new(1.0)),
            (2, pstore_format::Impact::new(2.0)),
        ]),
    );
    assert_eq!(d.footprint(), 3 + 20 + 5 + 20 + 16);
}
