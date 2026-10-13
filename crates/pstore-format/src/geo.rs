//! Locations and the distance between them (M66).
//!
//! ⚠️ **One function for the filter and the boost**, so a radius clause and a geo decay can
//! never disagree about how far two points are. The Earth is a sphere of
//! [`EARTH_RADIUS_M`]: the error against WGS-84 is up to about 0.5%.

use crate::Value;

/// The mean Earth radius, in meters (IUGG).
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// A value's `(lat, lon)` in degrees, if it is a location: an array of exactly two numbers,
/// finite, `lat` in −90..=90 and `lon` in −180..=180. Anything else is no location.
#[must_use]
pub fn location(v: &Value) -> Option<(f64, f64)> {
    let Value::Array(items) = v else { return None };
    let [a, b] = items.as_slice() else {
        return None;
    };
    let (lat, lon) = (number(a)?, number(b)?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

fn number(v: &Value) -> Option<f64> {
    match v {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a coordinate in range is far below 2^53"
        )]
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

/// The great-circle (haversine) distance between two `(lat, lon)` points in degrees, in
/// meters. Symmetric, and 0 for the same point.
#[must_use]
pub fn distance_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (p1, p2) = (a.0.to_radians(), b.0.to_radians());
    let dp = (b.0 - a.0).to_radians();
    let dl = (b.1 - a.1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    // Rounding can push `h` a hair past 1 for antipodes.
    2.0 * EARTH_RADIUS_M * h.clamp(0.0, 1.0).sqrt().asin()
}
