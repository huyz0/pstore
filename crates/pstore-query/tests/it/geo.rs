//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M66: `GeoRadius` and `GeoBox` as predicates.

use pstore_format::geo::distance_m;
use pstore_format::{Value, Zones};
use pstore_query::{Op, Predicate, condition};
use std::collections::BTreeMap;

fn at(lat: f64, lon: f64) -> BTreeMap<String, Value> {
    BTreeMap::from([(
        "loc".to_owned(),
        Value::Array(vec![Value::Float(lat), Value::Float(lon)]),
    )])
}

fn with(v: Value) -> BTreeMap<String, Value> {
    BTreeMap::from([("loc".to_owned(), v)])
}

fn radius(lat: f64, lon: f64, m: f64) -> Predicate {
    Predicate::GeoRadius("loc".to_owned(), lat, lon, m)
}

fn gbox(s: f64, w: f64, n: f64, e: f64) -> Predicate {
    Predicate::GeoBox("loc".to_owned(), s, w, n, e)
}

fn admits(p: &Predicate, a: &BTreeMap<String, Value>) -> bool {
    p.admits("x", a)
}

#[test]
fn geo_clauses_admit_what_they_define() {
    // A radius is inclusive at exactly its distance, and excludes a meter beyond.
    let d = distance_m((10.0, 10.0), (10.5, 10.5));
    assert!(admits(&radius(10.0, 10.0, d), &at(10.5, 10.5)));
    assert!(!admits(&radius(10.0, 10.0, d - 1.0), &at(10.5, 10.5)));
    assert!(admits(&radius(10.0, 10.0, 0.0), &at(10.0, 10.0)));

    // A box includes its edges and excludes just outside them.
    let b = gbox(10.0, 20.0, 30.0, 40.0);
    for (lat, lon) in [(10.0, 20.0), (30.0, 40.0), (10.0, 40.0), (20.0, 30.0)] {
        assert!(admits(&b, &at(lat, lon)), "{lat} {lon}");
    }
    for (lat, lon) in [
        (9.999, 30.0),
        (30.001, 30.0),
        (20.0, 19.999),
        (20.0, 40.001),
    ] {
        assert!(!admits(&b, &at(lat, lon)), "{lat} {lon}");
    }

    // West greater than east crosses the antimeridian: both sides, nothing between.
    let across = gbox(-10.0, 170.0, 10.0, -170.0);
    assert!(admits(&across, &at(0.0, 175.0)));
    assert!(admits(&across, &at(0.0, -175.0)));
    assert!(admits(&across, &at(0.0, 180.0)));
    assert!(admits(&across, &at(0.0, 170.0)));
    assert!(admits(&across, &at(0.0, -170.0)));
    assert!(!admits(&across, &at(0.0, 0.0)));
    assert!(!admits(&across, &at(0.0, 169.9)));
    assert!(!admits(&across, &at(0.0, -169.9)));
    assert!(!admits(&across, &at(11.0, 175.0)));

    // West equal to east is one meridian, not the world: it is not "across".
    let line = gbox(-10.0, 30.0, 10.0, 30.0);
    assert!(admits(&line, &at(0.0, 30.0)));
    assert!(!admits(&line, &at(0.0, 31.0)));
    assert!(!admits(&line, &at(0.0, -150.0)));

    // No location: false, for both clauses, and absent.
    let nones = [
        with(Value::Array(vec![Value::Float(1.0)])),
        with(Value::Array(vec![Value::Float(1.0); 3])),
        with(Value::Str("1,1".into())),
        with(Value::Float(1.0)),
        at(91.0, 0.0),
        at(0.0, 181.0),
        BTreeMap::new(),
    ];
    let everywhere = radius(0.0, 0.0, 20_015_087.0);
    let world = gbox(-90.0, -180.0, 90.0, 180.0);
    for n in &nones {
        assert!(!admits(&everywhere, n), "{n:?}");
        assert!(!admits(&world, n), "{n:?}");
        // `Not` is two-valued here: it admits what the clause does not.
        assert!(admits(&Predicate::Not(Box::new(everywhere.clone())), n));
    }
    // ...and the whole world admits any location, integers included.
    let ints = with(Value::Array(vec![Value::Int(-90), Value::Int(180)]));
    assert!(admits(&world, &ints));
    assert!(admits(&everywhere, &ints));
}

#[test]
fn geo_clauses_prune_nothing_and_round_trip() {
    let zones = [
        Zones::default(),
        Zones {
            complete: true,
            ..Zones::default()
        },
    ];
    for z in &zones {
        assert!(radius(0.0, 0.0, 1.0).could_admit(z));
        assert!(gbox(0.0, 0.0, 1.0, 1.0).could_admit(z));
    }
    // Under an `And`, the other clause still prunes.
    let mut z = Zones::default();
    z.ints.insert("n".to_owned(), (5, 5));
    z.complete = true;
    let and = Predicate::And(vec![
        radius(0.0, 0.0, 1.0),
        Predicate::Cmp("n".to_owned(), Op::Eq, Value::Int(6)),
    ]);
    assert!(!and.could_admit(&z));

    for p in [
        radius(48.8566, 2.3522, 1234.5),
        gbox(-10.0, 170.0, 10.0, -170.0),
        Predicate::Not(Box::new(radius(-0.0, 180.0, 0.0))),
    ] {
        let enc = condition::encode(&p);
        assert_eq!(condition::decode(&enc), Some(p.clone()));
        // Every truncation decodes to nothing, and never to another predicate.
        for cut in (0..enc.len()).step_by(2) {
            assert_eq!(condition::decode(&enc[..cut]), None, "{cut} of {enc}");
        }
    }
}
