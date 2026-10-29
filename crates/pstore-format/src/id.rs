//! Typed document ids (M176): an index's ids are strings, `uint64`s or `uuid`s, and a typed
//! one is held as a string naming its kind -- NUL, a kind letter, then fixed-width lowercase
//! hex -- so it orders as its values do, wherever ids are ordered as strings.
//!
//! ⚠️ A string id never begins with NUL (the door refuses one), so no string id takes a typed
//! form, and the form alone says what an id is: an answer needs no schema.

use crate::Value;
use crate::typed::Kind;

/// What leads a typed id's form.
const LEAD: char = '\0';
/// The kind letter of a `uint64` id.
const UINT: char = 'u';
/// The kind letter of a `uuid` id.
const UUID: char = 'x';

/// A `uint64` id's form: 16 hex digits.
#[must_use]
pub fn uint(n: u64) -> String {
    format!("{LEAD}{UINT}{n:016x}")
}

/// A `uuid` id's form: 32 hex digits.
#[must_use]
pub fn uuid(u: &[u8; 16]) -> String {
    let mut s = String::with_capacity(34);
    s.push(LEAD);
    s.push(UUID);
    for b in u {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Whether `id` may be a string id: one not beginning with NUL.
#[must_use]
pub fn is_string(id: &str) -> bool {
    !id.starts_with(LEAD)
}

/// An id's value: [`Value::Uint`] or [`Value::Uuid`] for a typed form, else the string.
#[must_use]
pub fn value(id: &str) -> Value {
    let Some(rest) = id.strip_prefix(LEAD) else {
        return Value::Str(id.to_owned());
    };
    let mut chars = rest.chars();
    // Lowercase hex digits only: `from_str_radix` would take a sign or an uppercase digit,
    // and a second spelling of one value would be a second id.
    let digits = |hex: &str, n: usize| {
        hex.len() == n && hex.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    };
    let typed = match (chars.next(), chars.as_str()) {
        (Some(UINT), hex) if digits(hex, 16) => u64::from_str_radix(hex, 16).ok().map(Value::Uint),
        (Some(UUID), hex) if digits(hex, 32) => u128::from_str_radix(hex, 16)
            .ok()
            .map(|n| Value::Uuid(n.to_be_bytes())),
        _ => None,
    };
    typed.unwrap_or_else(|| Value::Str(id.to_owned()))
}

/// The kind of a held id: `Some` for a typed form, `None` for a string.
#[must_use]
pub fn kind(id: &str) -> Option<Kind> {
    match value(id) {
        Value::Uint(_) => Some(Kind::Uint64),
        Value::Uuid(_) => Some(Kind::Uuid),
        _ => None,
    }
}

/// An id as a person reads it: a `uint64` in decimal, a `uuid` lowercase and hyphenated.
#[must_use]
pub fn shown(id: &str) -> String {
    match value(id) {
        Value::Uint(n) => n.to_string(),
        Value::Uuid(u) => crate::typed::format_uuid(&u),
        _ => id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "assertions in tests"
    )]
    use super::*;

    #[test]
    fn a_typed_form_orders_as_its_values_and_answers_them() {
        let ns = [0, 2, 10, 255, 256, (1 << 53) + 1, u64::MAX];
        let held: Vec<String> = ns.iter().map(|&n| uint(n)).collect();
        assert!(held.windows(2).all(|w| w[0] < w[1]), "{held:?}");
        for (n, h) in ns.iter().zip(&held) {
            assert_eq!(h.len(), 18);
            assert_eq!(value(h), Value::Uint(*n));
            assert_eq!(kind(h), Some(Kind::Uint64));
            assert_eq!(shown(h), n.to_string());
        }
        let us = [[0u8; 16], [0x0a; 16], [0xa0; 16], [0xff; 16]];
        let held: Vec<String> = us.iter().map(uuid).collect();
        assert!(held.windows(2).all(|w| w[0] < w[1]), "{held:?}");
        for (u, h) in us.iter().zip(&held) {
            assert_eq!(h.len(), 34);
            assert_eq!(value(h), Value::Uuid(*u));
            assert_eq!(kind(h), Some(Kind::Uuid));
            assert_eq!(shown(h), crate::typed::format_uuid(u));
        }
        assert_eq!(uuid(&[0xab; 16]), format!("\0x{}", "ab".repeat(16)));
        assert_eq!(uint(0xab), "\0u00000000000000ab");
    }

    #[test]
    fn anything_else_is_a_string() {
        for s in [
            "",
            "abc",
            "42",
            "u0000000000000002",
            "\0",
            "\0u",
            "\0u000000000000002",
            "\0u00000000000000002",
            "\0u000000000000000g",
            "\0x00",
            "\0u+000000000000002",
            "\0u00000000000000AB",
            "\0y0000000000000002",
        ] {
            assert_eq!(value(s), Value::Str(s.to_owned()), "{s:?}");
            assert_eq!(kind(s), None, "{s:?}");
            assert_eq!(shown(s), s, "{s:?}");
        }
        assert!(is_string("abc") && is_string(""));
        assert!(!is_string("\0abc") && !is_string(&uint(1)));
    }
}
