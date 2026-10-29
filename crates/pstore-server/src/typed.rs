//! The attribute types a write declares in its `schema` (M9h.3, M171, M172): `datetime`,
//! `uuid` and `bytes`, each a JSON string, and `uint64`, a JSON integer -- or an array of
//! them, read as the declared type.

use pstore_format::Value;
use std::collections::BTreeMap;

/// A declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Declared {
    /// RFC 3339, UTC (M9h.3).
    DateTime,
    /// 8-4-4-4-12 hex digits (M171).
    Uuid,
    /// Standard padded base64, at most [`pstore_format::typed::MAX_BYTES`] (M171).
    Bytes,
    /// A JSON integer in `0..=u64::MAX` (M172).
    Uint64,
}

/// What a write declares: an attribute name to its type.
pub(crate) type Declarations = BTreeMap<String, Declared>;

impl Declared {
    /// The type a declaration names, or `None`.
    pub(crate) fn named(ty: &serde_json::Value) -> Option<Self> {
        match ty.as_str()? {
            "datetime" => Some(Self::DateTime),
            "uuid" => Some(Self::Uuid),
            "bytes" => Some(Self::Bytes),
            "uint64" => Some(Self::Uint64),
            _ => None,
        }
    }

    /// A value under a name declared `self`: a scalar or an array of them, each a string.
    pub(crate) fn read(self, value: &serde_json::Value) -> Result<Value, String> {
        match value {
            serde_json::Value::Array(items) => items
                .iter()
                .map(|x| self.one(x))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            _ => self.one(value),
        }
    }

    fn one(self, x: &serde_json::Value) -> Result<Value, String> {
        let s = x.as_str();
        match self {
            Self::DateTime => s
                .and_then(pstore_format::datetime::parse)
                .map(Value::DateTime)
                .ok_or_else(|| {
                    format!(
                        "is declared a datetime and {x} is not one: RFC 3339, \
                         YYYY-MM-DDTHH:MM:SS[.ffffff] and Z or ±HH:MM, years 0000 to 9999 in UTC"
                    )
                }),
            Self::Uuid => s
                .and_then(pstore_format::typed::parse_uuid)
                .map(Value::Uuid)
                .ok_or_else(|| {
                    format!("is declared a uuid and {x} is not one: 8-4-4-4-12 hexadecimal digits")
                }),
            Self::Bytes => s
                .and_then(pstore_format::typed::parse_bytes)
                .ok_or_else(|| format!("is declared bytes and {x} is not standard padded base64"))
                .and_then(bounded),
            // `as_u64` is `None` for a negative integer and for whatever serde_json read as
            // a float, `5.0` and `1e3` among them.
            Self::Uint64 => x.as_u64().map(Value::Uint).ok_or_else(|| {
                format!(
                    "is declared uint64 and {x} is not one: an integer from 0 to {}",
                    u64::MAX
                )
            }),
        }
    }
}

/// Bytes within [`pstore_format::typed::MAX_BYTES`].
fn bounded(b: Vec<u8>) -> Result<Value, String> {
    let max = pstore_format::typed::MAX_BYTES;
    if b.len() > max {
        return Err(format!(
            "is declared bytes and holds {} bytes; the most is {max}",
            b.len()
        ));
    }
    Ok(Value::Bytes(b))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "assertions in tests are the reporting mechanism"
    )]
    use super::*;
    use serde_json::json;

    #[test]
    fn each_declared_type_reads_its_spelling_and_refuses_the_rest() {
        assert_eq!(
            Declared::named(&json!("datetime")),
            Some(Declared::DateTime)
        );
        assert_eq!(Declared::named(&json!("uuid")), Some(Declared::Uuid));
        assert_eq!(Declared::named(&json!("bytes")), Some(Declared::Bytes));
        assert_eq!(Declared::named(&json!("uint64")), Some(Declared::Uint64));
        assert_eq!(Declared::named(&json!("blob")), None);
        assert_eq!(Declared::named(&json!(1)), None);
        let u = "00000000-0000-4000-8000-000000000001";
        assert!(matches!(Declared::Uuid.read(&json!(u)), Ok(Value::Uuid(_))));
        assert_eq!(
            Declared::Bytes.read(&json!(["AAE="])).unwrap(),
            Value::Array(vec![Value::Bytes(vec![0, 1])])
        );
        assert_eq!(
            Declared::DateTime
                .read(&json!("1970-01-01T00:00:00Z"))
                .unwrap(),
            Value::DateTime(0)
        );
        for (d, bad) in [
            (Declared::Uuid, json!("x")),
            (Declared::Uuid, json!(7)),
            (Declared::Bytes, json!("AAE")),
            (Declared::DateTime, json!("yesterday")),
        ] {
            assert!(d.read(&bad).is_err(), "{d:?} read {bad}");
        }
        let max = pstore_format::typed::MAX_BYTES;
        assert!(bounded(vec![0; max]).is_ok());
        assert!(bounded(vec![0; max + 1]).is_err());
    }
}
