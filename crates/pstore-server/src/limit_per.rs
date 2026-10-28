//! A hard per-group limit on a relevance ranking (M146): of the window's rows, in final
//! order, at most `limit` of each group, until `top_k` are kept.
//!
//! ⚠️ **Exact over the window, and nothing beyond it**, as a boost is (M62): a row the engine
//! ranked below the window is never seen, so a window a few groups fill can return fewer
//! than `top_k` rows.

use pstore_format::Value;
use pstore_query::Key;
use std::collections::BTreeMap;

/// The most attributes a group is keyed by: `group_by`'s bound.
const MAX_ATTRIBUTES: usize = 8;

/// A parsed `limit_per`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LimitPer {
    attributes: Vec<String>,
    limit: usize,
    /// Candidates the engine ranks, and the limit walks.
    pub(crate) window: usize,
}

impl LimitPer {
    /// `raw` as a per-group limit for a query of `top_k`, or why it is not one. `shared` is
    /// the window a boost named: one window governs both.
    ///
    /// # Errors
    /// Every refusal of M146's Delta, as a message for a `400`.
    pub(crate) fn parse(
        raw: &serde_json::Value,
        top_k: usize,
        shared: Option<usize>,
    ) -> Result<Self, String> {
        let o = raw
            .as_object()
            .ok_or("limit_per is {\"attributes\", \"limit\", \"window\"}")?;
        if let Some(k) = o
            .keys()
            .find(|k| !["attributes", "limit", "window"].contains(&k.as_str()))
        {
            return Err(format!(
                "{k:?} is not a limit_per field; attributes, limit and window are"
            ));
        }
        let wrong_attributes =
            || format!("attributes is 1 to {MAX_ATTRIBUTES} distinct attribute names");
        let names = o
            .get("attributes")
            .and_then(serde_json::Value::as_array)
            .filter(|a| (1..=MAX_ATTRIBUTES).contains(&a.len()))
            .ok_or_else(wrong_attributes)?;
        let mut attributes: Vec<String> = Vec::with_capacity(names.len());
        for name in names {
            match name.as_str() {
                Some(n) if !attributes.iter().any(|a| a == n) => attributes.push(n.to_owned()),
                _ => return Err(wrong_attributes()),
            }
        }
        let limit = o
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .filter(|l| *l >= 1)
            .and_then(|l| usize::try_from(l).ok())
            .ok_or("limit is an integer of at least 1")?;
        let window = match o.get("window") {
            None => shared.unwrap_or_else(|| crate::boost::default_window(top_k)),
            Some(w) => {
                let w = w
                    .as_u64()
                    .and_then(|w| usize::try_from(w).ok())
                    .ok_or("window is a whole number")?;
                if let Some(s) = shared.filter(|s| *s != w) {
                    return Err(format!(
                        "window {w} differs from the boost's {s}; one window governs both"
                    ));
                }
                w
            }
        };
        if window < top_k || window > crate::boost::MAX_WINDOW {
            return Err(format!(
                "window is from top_k ({top_k}) to {}, not {window}",
                crate::boost::MAX_WINDOW
            ));
        }
        Ok(Self {
            attributes,
            limit,
            window,
        })
    }
}

/// `rows`, in their order, less each row whose group already holds `per.limit` kept rows,
/// stopping at `top_k` kept. `attrs` reads a row's stored attributes.
pub(crate) fn per_group<T>(
    rows: Vec<T>,
    attrs: impl Fn(&T) -> &BTreeMap<String, Value>,
    per: &LimitPer,
    top_k: usize,
) -> Vec<T> {
    let mut held: BTreeMap<Vec<Key>, usize> = BTreeMap::new();
    let mut kept = Vec::with_capacity(top_k.min(rows.len()));
    for row in rows {
        if kept.len() == top_k {
            break;
        }
        let a = attrs(&row);
        let group: Vec<Key> = per.attributes.iter().map(|n| Key::of(a.get(n))).collect();
        let count = held.entry(group).or_insert(0);
        if *count < per.limit {
            *count += 1;
            kept.push(row);
        }
    }
    kept
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;
    use pstore_format::Value;
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};

    type Row = (String, BTreeMap<String, Value>);

    /// The reference's own normalization: a string per value, `1` and `1.0` alike.
    fn norm(v: Option<&Value>) -> String {
        match v {
            None => "-".to_owned(),
            Some(Value::Int(n)) => format!("n{n}"),
            Some(Value::Uint(n)) => format!("n{n}"),
            Some(Value::Float(f)) if f.fract() == 0.0 && f.abs() < 1e15 => {
                format!("n{}", *f as i64)
            }
            Some(Value::Float(f)) => format!("f{f}"),
            Some(Value::Str(s)) => format!("s{s:?}"),
            Some(Value::Bool(b)) => format!("b{b}"),
            Some(Value::DateTime(t)) => format!("t{t}"),
            Some(Value::Uuid(u)) => format!("u{u:?}"),
            Some(Value::Bytes(b)) => format!("y{b:?}"),
            Some(Value::Array(a)) => format!(
                "[{}]",
                a.iter()
                    .map(|v| norm(Some(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    fn reference(rows: &[Row], attrs: &[&str], m: usize, top_k: usize) -> Vec<String> {
        let mut seen: HashMap<Vec<String>, usize> = HashMap::new();
        let mut kept = Vec::new();
        for (id, a) in rows {
            if kept.len() == top_k {
                break;
            }
            let key: Vec<String> = attrs.iter().map(|n| norm(a.get(*n))).collect();
            let c = seen.entry(key).or_insert(0);
            if *c < m {
                *c += 1;
                kept.push(id.clone());
            }
        }
        kept
    }

    fn any_value(state: u64) -> Option<Value> {
        let pick = (state >> 8) as i64 % 3;
        match state % 7 {
            0 => None,
            1 => Some(Value::Int(pick)),
            #[allow(clippy::cast_precision_loss, reason = "a small fixture value")]
            2 => Some(Value::Float(pick as f64)),
            3 => Some(Value::Float(0.5)),
            4 => Some(Value::Str(["a", "b", ""][pick as usize].to_owned())),
            5 => Some(Value::Array(vec![
                Value::Int(pick),
                Value::Str("x".to_owned()),
            ])),
            _ => Some(Value::Bool(pick == 0)),
        }
    }

    fn per(attrs: &[&str], m: u64) -> LimitPer {
        LimitPer::parse(&json!({"attributes": attrs, "limit": m}), 1, None).unwrap()
    }

    #[test]
    fn the_cut_equals_a_reference_over_random_rows() {
        let mut state = 0xA076_1D64_78BD_642F_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let names = ["g0", "g1", "g2"];
        for _ in 0..60 {
            let n = (next() % 40) as usize;
            let rows: Vec<Row> = (0..n)
                .map(|i| {
                    let mut a = BTreeMap::new();
                    for name in names {
                        if let Some(v) = any_value(next()) {
                            a.insert(name.to_owned(), v);
                        }
                    }
                    (format!("r{i}"), a)
                })
                .collect();
            for k in 1..=3 {
                for m in 1..=4 {
                    for top_k in 1..=n + 2 {
                        let p = per(&names[..k], m as u64);
                        let got: Vec<String> = per_group(rows.clone(), |r: &Row| &r.1, &p, top_k)
                            .into_iter()
                            .map(|r| r.0)
                            .collect();
                        assert_eq!(
                            got,
                            reference(&rows, &names[..k], m, top_k),
                            "k {k}, m {m}, top_k {top_k}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn one_and_one_point_zero_are_one_group_and_absent_is_a_group() {
        let row = |id: &str, v: Option<Value>| {
            let mut a = BTreeMap::new();
            if let Some(v) = v {
                a.insert("g".to_owned(), v);
            }
            (id.to_owned(), a)
        };
        let rows = vec![
            row("a", Some(Value::Int(1))),
            row("b", Some(Value::Float(1.0))),
            row("c", None),
            row("d", None),
            row("e", Some(Value::Int(2))),
        ];
        let got: Vec<String> = per_group(rows, |r: &Row| &r.1, &per(&["g"], 1), 10)
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(got, ["a", "c", "e"]);
    }

    #[test]
    fn the_window_defaults_as_a_boosts_and_is_bounded() {
        let raw = json!({"attributes": ["g"], "limit": 1});
        assert_eq!(LimitPer::parse(&raw, 10, None).unwrap().window, 100);
        assert_eq!(LimitPer::parse(&raw, 300, None).unwrap().window, 600);
        assert_eq!(LimitPer::parse(&raw, 10, Some(40)).unwrap().window, 40);
        let named = json!({"attributes": ["g"], "limit": 1, "window": 50});
        assert_eq!(LimitPer::parse(&named, 10, None).unwrap().window, 50);
        assert!(LimitPer::parse(&named, 60, None).is_err(), "below top_k");
        assert!(
            LimitPer::parse(
                &json!({"attributes": ["g"], "limit": 1, "window": 1001}),
                10,
                None
            )
            .is_err()
        );
        assert!(
            LimitPer::parse(&named, 10, Some(40)).is_err(),
            "a boost's other window"
        );
        assert!(
            LimitPer::parse(&named, 10, Some(50)).is_ok(),
            "the same window"
        );
    }

    #[test]
    fn what_cannot_mean_a_per_group_limit_is_refused() {
        let nine: Vec<String> = (0..9).map(|i| format!("a{i}")).collect();
        let eight: Vec<String> = (0..8).map(|i| format!("a{i}")).collect();
        for raw in [
            json!(2),
            json!({"limit": 1}),
            json!({"attributes": ["g"]}),
            json!({"attributes": [], "limit": 1}),
            json!({"attributes": nine, "limit": 1}),
            json!({"attributes": ["g", "g"], "limit": 1}),
            json!({"attributes": [1], "limit": 1}),
            json!({"attributes": "g", "limit": 1}),
            json!({"attributes": ["g"], "limit": 0}),
            json!({"attributes": ["g"], "limit": 1.5}),
            json!({"attributes": ["g"], "limit": -1}),
            json!({"attributes": ["g"], "limit": 1, "window": 20.5}),
            json!({"attributes": ["g"], "limit": 1, "x": 1}),
        ] {
            assert!(LimitPer::parse(&raw, 10, None).is_err(), "{raw}");
        }
        assert!(LimitPer::parse(&json!({"attributes": eight, "limit": 1}), 10, None).is_ok());
    }
}
