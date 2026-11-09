//! The JSON Schema subset `api/openapi.json` may use, and a validator for exactly it (M220).
//!
//! ⚠️ **A keyword outside [`KEYWORDS`] is an error, never ignored.** A validator that skips
//! what it does not know passes a schema it never checked: `not` or `pattern` written into the
//! contract would read as enforced and enforce nothing. [`keywords_in`] walks the whole
//! document, so a new keyword is refused before any response is validated against it.

use serde_json::Value;

/// Every keyword a schema in the contract may carry. Annotations are listed with the
/// assertions because the walk refuses anything else.
pub(crate) const KEYWORDS: &[&str] = &[
    // assertions
    "$ref",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "oneOf",
    "format",
    // annotations
    "description",
    "default",
    "x-pstore-status",
];

/// Every keyword under `schema` that [`KEYWORDS`] does not hold, with where it is.
pub(crate) fn keywords_in(schema: &Value, at: &str, out: &mut Vec<String>) {
    let Value::Object(o) = schema else {
        out.push(format!("{at}: a schema is an object"));
        return;
    };
    for (k, v) in o {
        if !KEYWORDS.contains(&k.as_str()) {
            out.push(format!("{at}: keyword {k:?} is not validated"));
            continue;
        }
        match k.as_str() {
            "properties" => {
                for (name, s) in v.as_object().into_iter().flatten() {
                    keywords_in(s, &format!("{at}.properties.{name}"), out);
                }
            }
            "items" => keywords_in(v, &format!("{at}.items"), out),
            "additionalProperties" if v.is_object() => {
                keywords_in(v, &format!("{at}.additionalProperties"), out);
            }
            "oneOf" => {
                for (i, s) in v.as_array().into_iter().flatten().enumerate() {
                    keywords_in(s, &format!("{at}.oneOf[{i}]"), out);
                }
            }
            "format" if v != "date-time" => out.push(format!("{at}: format {v} is not validated")),
            _ => {}
        }
    }
}

/// The schema a `$ref` names, inside `root`.
fn resolve<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    root.pointer(r.strip_prefix('#')?)
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.is_u64() || v.is_i64(),
        _ => false,
    }
}

/// An RFC 3339 time as the server writes it: `YYYY-MM-DDTHH:MM:SS`, a fraction, and `Z`.
fn is_date_time(s: &str) -> bool {
    let b = s.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    b.len() >= 20
        && digits(0..4)
        && b.get(4) == Some(&b'-')
        && digits(5..7)
        && b.get(7) == Some(&b'-')
        && digits(8..10)
        && b.get(10) == Some(&b'T')
        && digits(11..13)
        && b.get(13) == Some(&b':')
        && digits(14..16)
        && b.get(16) == Some(&b':')
        && digits(17..19)
        && s.ends_with('Z')
}

/// Every way `v` fails `schema`, each with the JSON path where it does.
pub(crate) fn validate(root: &Value, schema: &Value, v: &Value, at: &str, out: &mut Vec<String>) {
    let Value::Object(s) = schema else {
        out.push(format!("{at}: the schema is not an object"));
        return;
    };
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        match resolve(root, r) {
            Some(target) => validate(root, target, v, at, out),
            None => out.push(format!("{at}: {r} does not resolve")),
        }
    }
    if let Some(t) = s.get("type") {
        let ok = match t {
            Value::String(t) => type_matches(t, v),
            Value::Array(ts) => ts
                .iter()
                .filter_map(Value::as_str)
                .any(|t| type_matches(t, v)),
            _ => false,
        };
        if !ok {
            out.push(format!("{at}: {v} is not of type {t}"));
            return;
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        out.push(format!(
            "{at}: {v} is not one of {}",
            Value::Array(e.clone())
        ));
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = s.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            out.push(format!("{at}: {v} is below its minimum {min}"));
        }
        if let Some(max) = s.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            out.push(format!("{at}: {v} is above its maximum {max}"));
        }
    }
    if let (Some(text), Some("date-time")) = (v.as_str(), s.get("format").and_then(Value::as_str))
        && !is_date_time(text)
    {
        out.push(format!("{at}: {text:?} is not an RFC 3339 time"));
    }
    if let Some(a) = v.as_array() {
        if let Some(min) = s.get("minItems").and_then(Value::as_u64)
            && (a.len() as u64) < min
        {
            out.push(format!("{at}: {} items, fewer than {min}", a.len()));
        }
        if let Some(max) = s.get("maxItems").and_then(Value::as_u64)
            && (a.len() as u64) > max
        {
            out.push(format!("{at}: {} items, more than {max}", a.len()));
        }
        if let Some(items) = s.get("items") {
            for (i, e) in a.iter().enumerate() {
                validate(root, items, e, &format!("{at}[{i}]"), out);
            }
        }
    }
    if let Some(o) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for r in s
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(r) = r.as_str()
                && !o.contains_key(r)
            {
                out.push(format!("{at}: required {r:?} is missing"));
            }
        }
        for (k, e) in o {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => validate(root, ps, e, &format!("{at}.{k}"), out),
                None => match s.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        out.push(format!("{at}: {k:?} is not a property the contract names"));
                    }
                    Some(extra @ Value::Object(_)) => {
                        validate(root, extra, e, &format!("{at}.{k}"), out);
                    }
                    _ => {}
                },
            }
        }
    }
    if let Some(alts) = s.get("oneOf").and_then(Value::as_array) {
        let mut why = Vec::new();
        let matched = alts
            .iter()
            .filter(|alt| {
                let mut errs = Vec::new();
                validate(root, alt, v, at, &mut errs);
                why.extend(errs.iter().cloned());
                errs.is_empty()
            })
            .count();
        if matched != 1 {
            out.push(format!(
                "{at}: matches {matched} of oneOf's {} schemas, not exactly one: {why:?}",
                alts.len()
            ));
        }
    }
}

/// Every way `v` fails `schema`, or none.
pub(crate) fn errors(root: &Value, schema: &Value, v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    validate(root, schema, v, "$", &mut out);
    out
}
