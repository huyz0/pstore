//! The spellings of the declared types a JSON string carries (M171): a UUID's and bytes'.
//!
//! ⚠️ **One spelling in, one out.** A UUID is read as 8-4-4-4-12 hex digits of either case
//! and answered lowercase; bytes are read and answered as standard padded base64 (RFC 4648
//! §4). Anything else is refused, never repaired: a filter literal that does not parse
//! matches no row of the type, and a write carrying one is refused.

use base64::Engine as _;

/// The most bytes one `bytes` value holds.
pub const MAX_BYTES: usize = 4096;

/// A type a write declares for an attribute (M9h.3, M171, M172), and an index records (M173).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// RFC 3339, UTC: [`crate::Value::DateTime`].
    DateTime,
    /// 8-4-4-4-12 hex digits: [`crate::Value::Uuid`].
    Uuid,
    /// Standard padded base64: [`crate::Value::Bytes`].
    Bytes,
    /// A JSON integer in `0..=u64::MAX`: [`crate::Value::Uint`].
    Uint64,
}

impl Kind {
    /// Every kind, in code order.
    pub const ALL: [Self; 4] = [Self::DateTime, Self::Uuid, Self::Bytes, Self::Uint64];

    /// The name a write declares and a schema answers.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::DateTime => "datetime",
            Self::Uuid => "uuid",
            Self::Bytes => "bytes",
            Self::Uint64 => "uint64",
        }
    }

    /// The kind a name declares, or `None`.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Whether `v` is a value of this kind, or an array of them: what a recorded type admits.
    #[must_use]
    pub fn holds(self, v: &crate::Value) -> bool {
        use crate::Value;
        match (self, v) {
            // M175: a null element holds under every kind.
            (_, Value::Array(items)) => items
                .iter()
                .all(|i| matches!(i, Value::Null) || self.holds_scalar(i)),
            _ => self.holds_scalar(v),
        }
    }

    fn holds_scalar(self, v: &crate::Value) -> bool {
        use crate::Value;
        matches!(
            (self, v),
            (Self::DateTime, Value::DateTime(_))
                | (Self::Uuid, Value::Uuid(_))
                | (Self::Bytes, Value::Bytes(_))
                | (Self::Uint64, Value::Uint(_))
        )
    }
}

/// A UUID's 16 bytes from its hyphenated form, or `None`.
#[must_use]
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let b = s.as_bytes();
    if b.len() != 36 {
        return None;
    }
    if [8, 13, 18, 23].iter().any(|&i| b.get(i) != Some(&b'-')) {
        return None;
    }
    let digits: Vec<u8> = b
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(i, 8 | 13 | 18 | 23))
        .map(|(_, c)| *c)
        .collect();
    let mut out = [0u8; 16];
    for (o, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *o = pair
            .iter()
            .try_fold(0, |acc, &c| Some(acc * 16 + hex(c)?))?;
    }
    Some(out)
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A UUID as its lowercase hyphenated form.
#[must_use]
pub fn format_uuid(u: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    for (i, byte) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Bytes from standard padded base64, or `None`: not base64, or not canonical.
#[must_use]
pub fn parse_bytes(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Bytes as standard padded base64.
#[must_use]
pub fn format_bytes(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "assertions in tests are the reporting mechanism"
    )]
    use super::*;

    /// M173: a kind is named as a write declares it, and holds its own values only.
    #[test]
    fn a_kind_names_itself_and_holds_its_own_values() {
        use crate::Value;
        for k in Kind::ALL {
            assert_eq!(Kind::named(k.name()), Some(k));
        }
        assert_eq!(Kind::named("int"), None);
        assert!(Kind::Uuid.holds(&Value::Uuid([0; 16])));
        assert!(Kind::Uuid.holds(&Value::Array(vec![Value::Uuid([0; 16])])));
        assert!(Kind::Uuid.holds(&Value::Array(vec![])));
        assert!(!Kind::Uuid.holds(&Value::Str("x".into())));
        assert!(!Kind::Uuid.holds(&Value::Array(vec![Value::Uuid([0; 16]), Value::Int(1)])));
        assert!(Kind::Uint64.holds(&Value::Uint(1)) && !Kind::Uint64.holds(&Value::Int(1)));
        // M175: a null element holds under every kind; a wrong-kind one beside it does not.
        for k in Kind::ALL {
            assert!(k.holds(&Value::Array(vec![Value::Null])), "{k:?}");
            assert!(!k.holds(&Value::Array(vec![Value::Null, Value::Str("x".into())])));
        }
        assert!(Kind::Uuid.holds(&Value::Array(vec![Value::Null, Value::Uuid([0; 16])])));
        assert!(Kind::Bytes.holds(&Value::Bytes(vec![])) && !Kind::Bytes.holds(&Value::Uint(1)));
        assert!(Kind::DateTime.holds(&Value::DateTime(0)));
        assert!(!Kind::DateTime.holds(&Value::Int(0)));
    }

    /// M172: two uint64s order by value, and after every other type structurally.
    #[test]
    fn uint64s_order_by_value() {
        use crate::Value;
        assert!(Value::Uint(1) < Value::Uint(2));
        assert_ne!(Value::Uint(1), Value::Uint(2));
        assert!(Value::Bytes(vec![255]) < Value::Uint(0));
    }

    #[test]
    fn a_uuid_reads_either_case_and_answers_lowercase() {
        let u = parse_uuid("123E4567-e89b-12d3-A456-426614174000").unwrap();
        assert_eq!(u[0], 0x12);
        assert_eq!(u[15], 0x00);
        assert_eq!(format_uuid(&u), "123e4567-e89b-12d3-a456-426614174000");
    }

    #[test]
    fn a_malformed_uuid_is_none() {
        for bad in [
            "",
            "123e4567e89b12d3a456426614174000",
            "123e4567-e89b-12d3-a456-42661417400",
            "123e4567-e89b-12d3-a456-4266141740000",
            "123e4567-e89b-12d3-a456_426614174000",
            "g23e4567-e89b-12d3-a456-426614174000",
            "{23e4567-e89b-12d3-a456-42661417400}",
        ] {
            assert!(parse_uuid(bad).is_none(), "{bad:?} was read");
        }
    }

    #[test]
    fn bytes_round_trip_padded_base64_and_refuse_the_rest() {
        let b = vec![0u8, 1, 2, 250, 255];
        let s = format_bytes(&b);
        assert_eq!(s, "AAEC+v8=");
        assert_eq!(parse_bytes(&s).unwrap(), b);
        for bad in ["AAEC+v8", "AAEC+v8==", "AAEC-v8=", "not base64!"] {
            assert!(parse_bytes(bad).is_none(), "{bad:?} was read");
        }
        assert_eq!(parse_bytes("").unwrap(), Vec::<u8>::new());
    }
}
