//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M66: a location is a finite, in-range `[lat, lon]` pair, and a distance is the haversine
//! on a sphere of radius 6,371,008.8 m.

use pstore_format::Value;
use pstore_format::geo::{EARTH_RADIUS_M, distance_m, location};

fn pair(a: f64, b: f64) -> Value {
    Value::Array(vec![Value::Float(a), Value::Float(b)])
}

/// The formula, written out here and not shared with the code under test.
fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().asin()
}

#[test]
fn distance_is_haversine_on_the_sphere() {
    assert_eq!(EARTH_RADIUS_M, 6_371_008.8);
    // Paris to London.
    let (paris, london) = ((48.8566, 2.3522), (51.5074, -0.1278));
    let want = haversine(paris.0, paris.1, london.0, london.1);
    let got = distance_m(paris, london);
    assert!((got - want).abs() < 1.0, "{got} vs {want}");
    assert!((340_000.0..350_000.0).contains(&got), "{got}");
    // The same point.
    assert_eq!(distance_m(paris, paris), 0.0);
    // Antipodes are half the circumference.
    let half = std::f64::consts::PI * EARTH_RADIUS_M;
    assert!((distance_m((10.0, 20.0), (-10.0, -160.0)) - half).abs() < 1.0);
    // Across the antimeridian is the short way: 2 degrees of longitude at the equator.
    let across = distance_m((0.0, 179.0), (0.0, -179.0));
    let two_degrees = 2.0_f64.to_radians() * EARTH_RADIUS_M;
    assert!((across - two_degrees).abs() < 1.0, "{across}");
    // At a pole, longitude does not matter.
    let a = distance_m((90.0, 0.0), (0.0, 0.0));
    let b = distance_m((90.0, 123.0), (0.0, 0.0));
    assert!((a - b).abs() < 1e-6, "{a} {b}");
    assert!((a - half / 2.0).abs() < 1.0);
    // Symmetric.
    assert_eq!(distance_m(paris, london), distance_m(london, paris));
}

#[test]
fn location_is_a_finite_in_range_pair() {
    assert_eq!(location(&pair(48.5, -2.25)), Some((48.5, -2.25)));
    // Integers are numbers, and the bounds are inclusive.
    assert_eq!(
        location(&Value::Array(vec![Value::Int(-90), Value::Int(180)])),
        Some((-90.0, 180.0))
    );
    assert_eq!(location(&pair(90.0, -180.0)), Some((90.0, -180.0)));
    for bad in [
        pair(90.0001, 0.0),
        pair(-90.0001, 0.0),
        pair(0.0, 180.0001),
        pair(0.0, -180.0001),
        pair(f64::INFINITY, 0.0),
        Value::Array(vec![Value::Float(1.0)]),
        Value::Array(vec![Value::Float(1.0); 3]),
        Value::Array(vec![]),
        Value::Array(vec![Value::Str("1".into()), Value::Float(1.0)]),
        Value::Array(vec![Value::Bool(true), Value::Float(1.0)]),
        Value::Str("48.8,2.3".into()),
        Value::Float(1.0),
    ] {
        assert_eq!(location(&bad), None, "{bad:?}");
    }
}
