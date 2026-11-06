//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M195: a vector field's element types, and the components each holds.

use pstore_format::typed::{Kind, VECTOR_PREFIX, component, vector_key};

#[test]
fn the_vector_kinds_take_the_next_codes_and_are_no_attributes() {
    // A recorded type's HEAD byte is its place in `ALL` (M173): the four before keep theirs.
    let at = |k: Kind| Kind::ALL.iter().position(|x| *x == k).unwrap();
    assert_eq!(
        [
            Kind::DateTime,
            Kind::Uuid,
            Kind::Bytes,
            Kind::Uint64,
            Kind::F16,
            Kind::I8
        ]
        .map(at),
        [0, 1, 2, 3, 4, 5]
    );
    assert_eq!((Kind::F16.name(), Kind::I8.name()), ("f16", "i8"));
    assert!(Kind::F16.is_vector() && Kind::I8.is_vector());
    assert!(!Kind::ALL[..4].iter().any(|k| k.is_vector()));
    // No value of an attribute is one of a vector kind's.
    for v in [
        pstore_format::Value::Float(1.0),
        pstore_format::Value::Int(1),
        pstore_format::Value::Array(vec![pstore_format::Value::Float(1.0)]),
    ] {
        assert!(!Kind::F16.holds(&v) && !Kind::I8.holds(&v), "{v:?}");
    }
    assert_eq!(vector_key("vector"), "$vector.vector");
    assert_eq!(VECTOR_PREFIX, "$vector.");
}

#[test]
fn components_round_to_f16_or_must_be_i8_integers() {
    let f16 = |x: f32| component(Kind::F16, x);
    // Ties to even, at 1 and at the smallest subnormal's scale.
    assert_eq!(f16(1.0 + 1.0 / 2048.0), Some(1.0));
    assert_eq!(f16(1.0 + 3.0 / 2048.0), Some(1.0 + 2.0 / 1024.0));
    let tiny = 1.0 / 16_777_216.0;
    assert_eq!(f16(1.5 * tiny), Some(2.0 * tiny));
    assert_eq!(f16(2.5 * tiny), Some(2.0 * tiny));
    // The largest, what rounds to it, and the first that does not.
    assert_eq!(f16(65504.0), Some(65504.0));
    assert_eq!(f16(65519.0), Some(65504.0));
    assert_eq!(f16(-65519.0), Some(-65504.0));
    assert_eq!(f16(65520.0), None);
    assert_eq!(f16(-65520.0), None);
    assert_eq!(f16(f32::NAN), None);
    assert_eq!(f16(f32::INFINITY), None);
    assert!(f16(-0.0).unwrap().is_sign_negative());

    let i8 = |x: f32| component(Kind::I8, x);
    assert_eq!(i8(-128.0), Some(-128.0));
    assert_eq!(i8(127.0), Some(127.0));
    assert_eq!(i8(3.0), Some(3.0));
    for bad in [3.5, 128.0, -129.0, 0.25, f32::NAN, f32::INFINITY] {
        assert_eq!(i8(bad), None, "{bad}");
    }
    assert!(
        i8(-0.0).unwrap().is_sign_positive(),
        "i8 has no negative zero"
    );
    // An attribute's kind changes nothing.
    assert_eq!(
        component(Kind::Uuid, 1.0 + 1.0 / 2048.0),
        Some(1.0 + 1.0 / 2048.0)
    );
}
