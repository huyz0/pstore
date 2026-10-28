//! The spellings of the declared types a JSON string carries (M171): a UUID's and bytes'.
//!
//! ⚠️ **One spelling in, one out.** A UUID is read as 8-4-4-4-12 hex digits of either case
//! and answered lowercase; bytes are read and answered as standard padded base64 (RFC 4648
//! §4). Anything else is refused, never repaired: a filter literal that does not parse
//! matches no row of the type, and a write carrying one is refused.

use base64::Engine as _;

/// The most bytes one `bytes` value holds.
pub const MAX_BYTES: usize = 4096;

/// A UUID's 16 bytes from its hyphenated form, or `None`.
#[must_use]
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let b = s.as_bytes();
    if b.len() != 36 {
        return None;
    }
    let mut out = [0u8; 16];
    let mut n = 0;
    let mut i = 0;
    while i < 36 {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b.get(i) != Some(&b'-') {
                return None;
            }
            i += 1;
            continue;
        }
        let hi = hex(*b.get(i)?)?;
        let lo = hex(*b.get(i + 1)?)?;
        *out.get_mut(n)? = (hi << 4) | lo;
        n += 1;
        i += 2;
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
