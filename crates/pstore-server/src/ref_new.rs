//! `$ref_new` in write conditions (M133): a comparison whose value is
//! `{"$ref_new": "<attribute>"}` compares the current row with the operation's own new value.
//! Resolved here, at the door, once per operation, so the engine and the fold see ordinary
//! predicates: one per operation instead of one per request.

use crate::{ApiError, clause, write_filter};
use pstore_format::Value;
use pstore_query::{Op, Predicate};
use std::collections::BTreeMap;

/// A write condition as parsed: one predicate for every operation, or a template each
/// operation resolves.
pub(crate) enum WriteCond {
    All(Predicate),
    Each(Template),
}

/// A condition holding at least one reference.
pub(crate) enum Template {
    Fixed(Predicate),
    Ref(String, RefOp, String),
    And(Vec<Template>),
    Or(Vec<Template>),
    Not(Box<Template>),
}

/// The operators a reference may be the value of.
#[derive(Clone, Copy)]
pub(crate) enum RefOp {
    Eq,
    NotEq,
    Lt,
    Lte,
    Gt,
    Gte,
}

/// What one call's operations are conditioned on: one predicate (or none) for all, or one
/// each.
pub(crate) enum Conds {
    One(Option<Predicate>),
    Each(Vec<Predicate>),
}

/// `name`'s condition: a template when it holds a reference, else the filter as M13 parses
/// it -- which refuses a reference anywhere a template does not take one.
pub(crate) fn write_cond(name: &str, v: &serde_json::Value) -> Result<WriteCond, ApiError> {
    if !mentions(v) {
        return write_filter(name, v).map(WriteCond::All);
    }
    let t = template(v).map_err(|e| ApiError::bad_request(format!("{name}: {}", e.message)))?;
    if let Some(p) = costly(&t) {
        return Err(ApiError::bad_request(format!(
            "{name}: $ref_new cannot sit beside a Glob, IGlob, Regex, Fuzzy or token clause \
             ({p:?}): the condition differs per operation, and each would be compiled again"
        )));
    }
    // Every resolution has the same depth, so the empty one stands for all of them.
    let p = resolve(&t, "", &BTreeMap::new()).unwrap_or(Predicate::Or(Vec::new()));
    if pstore_query::condition::decode(&pstore_query::condition::encode(&p)).is_none() {
        return Err(ApiError::bad_request(format!(
            "{name}: the filter nests too deeply"
        )));
    }
    Ok(WriteCond::Each(t))
}

/// Whether any object in `v` has the key `$ref_new`.
fn mentions(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Array(items) => items.iter().any(mentions),
        serde_json::Value::Object(o) => o.contains_key("$ref_new") || o.values().any(mentions),
        _ => false,
    }
}

fn template(v: &serde_json::Value) -> Result<Template, ApiError> {
    let bad = |why: &str| ApiError::bad_request(format!("malformed filter {v}: {why}"));
    match v.as_array().map(Vec::as_slice) {
        Some([op, clauses]) if op == "And" || op == "Or" => {
            let list = clauses
                .as_array()
                .ok_or_else(|| bad("And and Or take an array of filters"))?;
            let parts = list.iter().map(template).collect::<Result<Vec<_>, _>>()?;
            Ok(if op == "And" {
                Template::And(parts)
            } else {
                Template::Or(parts)
            })
        }
        Some([op, inner]) if op == "Not" => Ok(Template::Not(Box::new(template(inner)?))),
        Some([attr, op, serde_json::Value::Object(o)]) if o.contains_key("$ref_new") => {
            let Some(serde_json::Value::String(name)) = o.get("$ref_new").filter(|_| o.len() == 1)
            else {
                return Err(bad("a reference is {\"$ref_new\": \"<attribute>\"}, alone"));
            };
            let reserved = |a: &str| a.is_empty() || a.starts_with('$');
            if reserved(name) {
                return Err(bad("$ref_new names a reserved attribute"));
            }
            let attr = attr
                .as_str()
                .filter(|a| !reserved(a))
                .ok_or_else(|| bad("the attribute is a string naming no reserved attribute"))?;
            let op = match op.as_str() {
                Some("Eq") => RefOp::Eq,
                Some("NotEq") => RefOp::NotEq,
                Some("Lt") => RefOp::Lt,
                Some("Lte") => RefOp::Lte,
                Some("Gt") => RefOp::Gt,
                Some("Gte") => RefOp::Gte,
                _ => {
                    return Err(bad(
                        "$ref_new is the value of Eq, NotEq, Lt, Lte, Gt or Gte only",
                    ));
                }
            };
            Ok(Template::Ref(attr.to_owned(), op, name.clone()))
        }
        _ => clause(v).map(Template::Fixed),
    }
}

/// A clause a reference may not sit beside, if `t` holds one.
fn costly(t: &Template) -> Option<&Predicate> {
    // A fixed part is one leaf clause (`template` walks `And`, `Or` and `Not` itself), which
    // is at most a negated one: `NotGlob` is `Not(Pattern)`.
    fn pred(p: &Predicate) -> Option<&Predicate> {
        match p {
            Predicate::Pattern(..)
            | Predicate::Fuzzy(..)
            | Predicate::FuzzySubstring(..)
            | Predicate::Tokens(..) => Some(p),
            Predicate::Not(p) => pred(p),
            _ => None,
        }
    }
    match t {
        Template::Fixed(p) => pred(p),
        Template::Ref(..) => None,
        Template::And(ts) | Template::Or(ts) => ts.iter().find_map(costly),
        Template::Not(t) => costly(t),
    }
}

/// `t` for the operation on `id` whose new values are `attrs`; `Err` names a reference that
/// resolved to an array.
fn resolve(t: &Template, id: &str, attrs: &BTreeMap<String, Value>) -> Result<Predicate, String> {
    Ok(match t {
        Template::Fixed(p) => p.clone(),
        Template::And(ts) => Predicate::And(each(ts, id, attrs)?),
        Template::Or(ts) => Predicate::Or(each(ts, id, attrs)?),
        Template::Not(t) => Predicate::Not(Box::new(resolve(t, id, attrs)?)),
        Template::Ref(attr, op, name) => {
            let v = if name == pstore_query::ID_ATTRIBUTE {
                Some(Value::Str(id.to_owned()))
            } else {
                attrs.get(name).cloned()
            };
            if matches!(v, Some(Value::Array(_))) {
                return Err(name.clone());
            }
            let cmp = |o: Op, v: Value| Predicate::Cmp(attr.clone(), o, v);
            match (op, v) {
                (RefOp::Eq, None) => Predicate::Absent(attr.clone()),
                (RefOp::NotEq, None) => Predicate::Not(Box::new(Predicate::Absent(attr.clone()))),
                // Ordered against nothing: false, as a type mismatch is.
                (_, None) => Predicate::Or(Vec::new()),
                (RefOp::Eq, Some(v)) => cmp(Op::Eq, v),
                (RefOp::NotEq, Some(v)) => Predicate::Not(Box::new(cmp(Op::Eq, v))),
                (RefOp::Lt, Some(v)) => cmp(Op::Lt, v),
                (RefOp::Lte, Some(v)) => cmp(Op::Lte, v),
                (RefOp::Gt, Some(v)) => cmp(Op::Gt, v),
                (RefOp::Gte, Some(v)) => cmp(Op::Gte, v),
            }
        }
    })
}

fn each(
    ts: &[Template],
    id: &str,
    attrs: &BTreeMap<String, Value>,
) -> Result<Vec<Predicate>, String> {
    ts.iter().map(|t| resolve(t, id, attrs)).collect()
}

/// The conditions of `ops` -- each an id and its new values -- under `name`'s condition `c`.
pub(crate) fn conds(
    name: &str,
    c: Option<WriteCond>,
    ops: &[(&str, &BTreeMap<String, Value>)],
) -> Result<Conds, ApiError> {
    match c {
        None => Ok(Conds::One(None)),
        Some(WriteCond::All(p)) => Ok(Conds::One(Some(p))),
        Some(WriteCond::Each(t)) => ops
            .iter()
            .map(|(id, attrs)| {
                resolve(&t, id, attrs).map_err(|r| {
                    ApiError::bad_request(format!(
                        "{name}: $ref_new {r:?} of operation {id} is an array; a reference \
                         must be a scalar"
                    ))
                })
            })
            .collect::<Result<_, _>>()
            .map(Conds::Each),
    }
}
