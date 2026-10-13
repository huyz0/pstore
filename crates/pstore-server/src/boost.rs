//! A ranked query's window, rescored by its candidates' attributes (M62).
//!
//! ⚠️ **Exact within the window, and nothing beyond it.** The engine ranks `window`
//! candidates and the server adds each one's terms: a document the engine ranked below the
//! window is never seen, as in Vespa's ranking phases. Depth is the engine's, because every
//! candidate's attributes already come back in the row round that resolves its id.

use pstore_format::Value;
use std::collections::BTreeMap;

/// Candidates a boost may rescore at most.
pub(crate) const MAX_WINDOW: usize = 1000;
/// Terms a boost may hold at most.
pub(crate) const MAX_TERMS: usize = 8;
/// The least window when the query names none.
const DEFAULT_WINDOW: usize = 100;

/// The window a query names none of, for `top_k`: twice it, so a boost can lift as well as
/// reorder, between [`DEFAULT_WINDOW`] and [`MAX_WINDOW`]. One function for a boost and a
/// diversify (M64), which share a window.
pub(crate) fn default_window(top_k: usize) -> usize {
    top_k.saturating_mul(2).clamp(DEFAULT_WINDOW, MAX_WINDOW)
}

/// A parsed `boost`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Boost {
    /// Candidates the engine ranks, and the boost rescores.
    pub(crate) window: usize,
    terms: Vec<Term>,
}

#[derive(Debug, Clone, PartialEq)]
struct Term {
    attr: String,
    weight: f64,
    f: Function,
}

#[derive(Debug, Clone, PartialEq)]
enum Function {
    Linear,
    Saturate {
        pivot: f64,
    },
    Decay {
        origin: Origin,
        scale: f64,
        shape: Shape,
    },
    Eq(Value),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Origin {
    /// For an `int` or `float`, in its unit.
    Number(f64),
    /// For a `datetime`, in microseconds; the scale is in seconds.
    Instant(i64),
    /// For a location, `(lat, lon)` in degrees; the scale is in meters (M66).
    Place(f64, f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Exp,
    Gauss,
    Linear,
}

/// `raw` as a boost for a query of `top_k`, or why it is not one.
///
/// # Errors
/// Every refusal of M62's rule 6, as a message for a `400`.
pub(crate) fn parse(
    raw: &serde_json::Value,
    top_k: usize,
    shared: Option<usize>,
) -> Result<Boost, String> {
    let o = raw.as_object().ok_or("boost is {\"window\", \"terms\"}")?;
    if let Some(k) = o.keys().find(|k| *k != "window" && *k != "terms") {
        return Err(format!("{k:?} is not a boost field; window and terms are"));
    }
    let window = match o.get("window") {
        // The window a diversify named, if it did, else the default (M64).
        None => shared.unwrap_or_else(|| default_window(top_k)),
        Some(w) => w
            .as_u64()
            .and_then(|w| usize::try_from(w).ok())
            .ok_or("window is a whole number")?,
    };
    if window < top_k || window > MAX_WINDOW {
        return Err(format!(
            "window is from top_k ({top_k}) to {MAX_WINDOW}, not {window}"
        ));
    }
    let terms = o
        .get("terms")
        .and_then(serde_json::Value::as_array)
        .filter(|t| (1..=MAX_TERMS).contains(&t.len()))
        .ok_or(format!("terms is an array of 1 to {MAX_TERMS} terms"))?
        .iter()
        .map(term)
        .collect::<Result<_, _>>()?;
    Ok(Boost { window, terms })
}

fn term(raw: &serde_json::Value) -> Result<Term, String> {
    let o = raw
        .as_object()
        .ok_or("a term is {\"attr\", \"fn\", \"weight\", ...}")?;
    let attr = o
        .get("attr")
        .and_then(serde_json::Value::as_str)
        .ok_or("a term names its attr")?;
    if attr.is_empty() || attr == pstore_query::ID_ATTRIBUTE || attr.starts_with('$') {
        return Err(format!(
            "attr {attr:?}: a boost reads an attribute, never empty, `id` or a `$` name"
        ));
    }
    let weight = o
        .get("weight")
        .and_then(serde_json::Value::as_f64)
        .filter(|w| w.is_finite())
        .ok_or(format!("attr {attr}: a term's weight is a finite number"))?;
    let name = o
        .get("fn")
        .and_then(serde_json::Value::as_str)
        .ok_or(format!("attr {attr}: a term names its fn"))?;
    let takes: &[&str] = match name {
        "linear" => &[],
        "saturate" => &["pivot"],
        "decay" => &["origin", "scale", "shape"],
        "eq" => &["value"],
        other => {
            return Err(format!("fn {other:?} is not linear, saturate, decay or eq"));
        }
    };
    if let Some(k) = o
        .keys()
        .find(|k| !matches!(k.as_str(), "attr" | "fn" | "weight") && !takes.contains(&k.as_str()))
    {
        return Err(format!("attr {attr}: {name} takes no {k:?}"));
    }
    let positive = |k: &str| {
        o.get(k)
            .and_then(serde_json::Value::as_f64)
            .filter(|x| x.is_finite() && *x > 0.0)
            .ok_or(format!("attr {attr}: {name}'s {k} is a number above 0"))
    };
    let f = match name {
        "saturate" => Function::Saturate {
            pivot: positive("pivot")?,
        },
        "decay" => {
            let origin = match o.get("origin") {
                Some(serde_json::Value::String(s)) => pstore_format::datetime::parse(s)
                    .map(Origin::Instant)
                    .ok_or(format!("attr {attr}: origin {s:?} is no RFC 3339 datetime"))?,
                Some(serde_json::Value::Array(pair)) => match pair
                    .iter()
                    .map(|x| x.as_f64().filter(|f| f.is_finite()))
                    .collect::<Option<Vec<f64>>>()
                    .as_deref()
                {
                    Some(&[lat, lon])
                        if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
                    {
                        Origin::Place(lat, lon)
                    }
                    _ => {
                        return Err(format!(
                            "attr {attr}: an origin pair is [lat, lon], -90 to 90 and -180 to 180"
                        ));
                    }
                },
                Some(v) => v
                    .as_f64()
                    .filter(|x| x.is_finite())
                    .map(Origin::Number)
                    .ok_or(format!(
                        "attr {attr}: origin is a finite number or an RFC 3339 datetime"
                    ))?,
                None => return Err(format!("attr {attr}: decay needs an origin")),
            };
            let shape = match o.get("shape").map(|s| s.as_str()) {
                None | Some(Some("exp")) => Shape::Exp,
                Some(Some("gauss")) => Shape::Gauss,
                Some(Some("linear")) => Shape::Linear,
                Some(_) => {
                    return Err(format!("attr {attr}: shape is exp, gauss or linear"));
                }
            };
            Function::Decay {
                origin,
                scale: positive("scale")?,
                shape,
            }
        }
        "eq" => Function::Eq(match o.get("value") {
            Some(serde_json::Value::String(s)) => Value::Str(s.clone()),
            Some(serde_json::Value::Bool(b)) => Value::Bool(*b),
            Some(serde_json::Value::Number(n)) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(
                    n.as_f64()
                        .ok_or(format!("attr {attr}: eq's value is out of range"))?,
                ),
            },
            _ => {
                return Err(format!(
                    "attr {attr}: eq's value is a string, number or bool"
                ));
            }
        }),
        _ => Function::Linear,
    };
    Ok(Term {
        attr: attr.to_owned(),
        weight,
        f,
    })
}

/// A number an `int` or `float` holds, for `linear`, `saturate` and a numeric decay.
fn number(v: &Value) -> Option<f64> {
    match v {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a boost scores in f64; an integer beyond 2^53 is ranked, not counted"
        )]
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// A filter's `Eq`: numbers exactly across int and float, a datetime against the instant a
/// string parses to, anything else structurally.
fn equal(a: &Value, want: &Value) -> bool {
    use pstore_format::{Number, cmp_numbers};
    match (a, want) {
        (Value::DateTime(t), Value::Str(s)) => pstore_format::datetime::parse(s) == Some(*t),
        _ => match (Number::of(a), Number::of(want)) {
            (Some(x), Some(y)) => cmp_numbers(x, y) == Some(std::cmp::Ordering::Equal),
            _ => a == want,
        },
    }
}

impl Function {
    /// The term's `f(x)`, or `None` when `x` is of a type it does not read: such a term
    /// contributes nothing.
    fn of(&self, x: &Value) -> Option<f64> {
        match self {
            Self::Linear => number(x),
            Self::Saturate { pivot } => number(x).map(|x| {
                let x = x.max(0.0);
                x / (x + pivot)
            }),
            Self::Decay {
                origin,
                scale,
                shape,
            } => {
                let d = match (origin, x) {
                    (Origin::Instant(o), Value::DateTime(t)) => {
                        #[expect(
                            clippy::cast_precision_loss,
                            reason = "microseconds within the datetime range fit f64's 2^53 \
                                      only to about 285 years; a decay's scale is far coarser"
                        )]
                        let micros = t.abs_diff(*o) as f64;
                        micros / 1e6
                    }
                    (Origin::Number(o), v) => (number(v)? - o).abs(),
                    (Origin::Place(lat, lon), v) => pstore_format::geo::distance_m(
                        (*lat, *lon),
                        pstore_format::geo::location(v)?,
                    ),
                    _ => return None,
                };
                let r = d / scale;
                Some(match shape {
                    Shape::Exp => 0.5f64.powf(r),
                    Shape::Gauss => 0.5f64.powf(r * r),
                    Shape::Linear => (1.0 - r / 2.0).max(0.0),
                })
            }
            Self::Eq(want) => Some(match x {
                Value::Array(items) => f64::from(u8::from(items.iter().any(|i| equal(i, want)))),
                v => f64::from(u8::from(equal(v, want))),
            }),
        }
    }
}

impl Boost {
    /// A candidate's final score: `score` and each term added in order, in `f64`.
    #[must_use]
    pub(crate) fn score(&self, score: f32, attrs: &BTreeMap<String, Value>) -> f32 {
        let mut total = f64::from(score);
        for t in &self.terms {
            // A term that overflows contributes nothing: every one finite, the sum can reach
            // infinity but never NaN, and the clamp below bounds it.
            if let Some(c) = attrs
                .get(&t.attr)
                .and_then(|x| t.f.of(x))
                .map(|f| t.weight * f)
                .filter(|c| c.is_finite())
            {
                total += c;
            }
        }
        let max = f64::from(f32::MAX);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a score is an f32 on the wire, as every fused score is; clamped first"
        )]
        let total = total.clamp(-max, max) as f32;
        total
    }
}
