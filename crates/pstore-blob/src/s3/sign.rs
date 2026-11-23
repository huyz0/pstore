//! AWS Signature Version 4, header form, for S3 (M286).
//!
//! In-crate rather than a dependency: the whole algorithm is a canonical string and four
//! HMACs, and the crates that offer it bring a credential chain and a clock library with
//! them. `ring` is already in the lockfile.
//!
//! ⚠️ **Encoding is shared with the request line.** A key is percent-encoded once, by
//! [`uri_encode`], and the same string goes on the wire and into the canonical request: two
//! encoders that disagree on one byte are a `403 SignatureDoesNotMatch` for exactly the keys
//! that hold that byte.

use ring::{digest, hmac};
use std::fmt::Write as _;

/// The empty body's SHA-256, which every bodiless request signs.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// An access key, its secret, and the session token temporary credentials carry.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// `AWS_ACCESS_KEY_ID`.
    pub key_id: String,
    /// `AWS_SECRET_ACCESS_KEY`.
    pub secret: String,
    /// `AWS_SESSION_TOKEN`, sent and signed as `x-amz-security-token` when present.
    pub token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    // ⚠️ Never the secret: a `Debug` of a config is how a key reaches a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// What a signature covers.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// `GET`, `PUT`, ...
    pub method: &'a str,
    /// The path as it goes on the wire, **already encoded** by [`uri_encode`].
    pub path: &'a str,
    /// Query parameters, raw; encoded and sorted here.
    pub query: &'a [(&'a str, &'a str)],
    /// Every signed header, **lowercase names**, `host` and `x-amz-date` among them.
    pub headers: &'a [(&'a str, &'a str)],
    /// The body's SHA-256 in hex, as sent in `x-amz-content-sha256`.
    pub payload_sha256: &'a str,
}

/// Where and when a signature is valid.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    /// `20150830T123600Z`, the value of `x-amz-date`.
    pub amz_date: &'a str,
    /// `us-east-1`.
    pub region: &'a str,
    /// `s3`.
    pub service: &'a str,
}

/// S3's URI encoding: unreserved bytes as they are, `/` kept when `keep_slash`, every other
/// byte as `%XX` in upper case.
#[must_use]
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 4);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(char::from(b));
        } else {
            // Infallible: writing to a String.
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The canonical query string: each pair encoded, sorted by key then value.
#[must_use]
pub fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// SHA-256 of `data`, hex.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn hmac(key: &[u8], data: &[u8]) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
}

/// The `Authorization` header's value for `req`.
#[must_use]
pub fn authorization(creds: &Credentials, scope: &Scope<'_>, req: &Request<'_>) -> String {
    let mut headers: Vec<(&str, &str)> = req.headers.to_vec();
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let mut canonical = String::with_capacity(256);
    canonical.push_str(req.method);
    canonical.push('\n');
    canonical.push_str(req.path);
    canonical.push('\n');
    canonical.push_str(&canonical_query(req.query));
    canonical.push('\n');
    let mut signed = String::with_capacity(64);
    for (i, (name, value)) in headers.iter().enumerate() {
        canonical.push_str(name);
        canonical.push(':');
        canonical.push_str(value.trim());
        canonical.push('\n');
        if i > 0 {
            signed.push(';');
        }
        signed.push_str(name);
    }
    canonical.push('\n');
    canonical.push_str(&signed);
    canonical.push('\n');
    canonical.push_str(req.payload_sha256);

    let date = scope.amz_date.get(..8).unwrap_or(scope.amz_date);
    let credential_scope = format!("{date}/{}/{}/aws4_request", scope.region, scope.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{credential_scope}\n{}",
        scope.amz_date,
        sha256_hex(canonical.as_bytes())
    );
    let k = hmac(format!("AWS4{}", creds.secret).as_bytes(), date.as_bytes());
    let k = hmac(k.as_ref(), scope.region.as_bytes());
    let k = hmac(k.as_ref(), scope.service.as_bytes());
    let k = hmac(k.as_ref(), b"aws4_request");
    let signature = hex(hmac(k.as_ref(), to_sign.as_bytes()).as_ref());
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed}, Signature={signature}",
        creds.key_id
    )
}

/// `x-amz-date` for `unix` seconds: `YYYYMMDDTHHMMSSZ`, UTC.
///
/// Civil-from-days (Howard Hinnant's algorithm), so the client needs no calendar crate:
/// the one date it formats is "now", in UTC, to the second.
#[must_use]
pub fn amz_date(unix: u64) -> String {
    let days = i64::try_from(unix / 86_400).unwrap_or(i64::MAX);
    let secs = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn signature(auth: &str) -> &str {
        auth.rsplit("Signature=").next().unwrap()
    }

    #[test]
    fn sigv4_matches_the_aws_examples() {
        // The SigV4 test suite's `get-vanilla`.
        let creds = Credentials {
            key_id: "AKIDEXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            token: None,
        };
        let auth = authorization(
            &creds,
            &Scope {
                amz_date: "20150830T123600Z",
                region: "us-east-1",
                service: "service",
            },
            &Request {
                method: "GET",
                path: "/",
                query: &[],
                headers: &[
                    ("host", "example.amazonaws.com"),
                    ("x-amz-date", "20150830T123600Z"),
                ],
                payload_sha256: EMPTY_SHA256,
            },
        );
        assert_eq!(
            signature(&auth),
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, "
        ));

        // The S3 documentation's GET Object with a range. Headers deliberately unsorted:
        // sorting is the signer's job.
        let creds = Credentials {
            key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            token: None,
        };
        let auth = authorization(
            &creds,
            &Scope {
                amz_date: "20130524T000000Z",
                region: "us-east-1",
                service: "s3",
            },
            &Request {
                method: "GET",
                path: "/test.txt",
                query: &[],
                headers: &[
                    ("x-amz-date", "20130524T000000Z"),
                    ("range", "bytes=0-9"),
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("x-amz-content-sha256", EMPTY_SHA256),
                ],
                payload_sha256: EMPTY_SHA256,
            },
        );
        assert_eq!(
            signature(&auth),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn keys_and_queries_are_encoded_the_way_s3_canonicalises_them() {
        assert_eq!(uri_encode("a/b c+d~_.-", true), "a/b%20c%2Bd~_.-");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(uri_encode("é", true), "%C3%A9");
        // Sorted by encoded key; an empty value keeps its `=`.
        assert_eq!(
            canonical_query(&[("prefix", "a/b"), ("delete", ""), ("list-type", "2")]),
            "delete=&list-type=2&prefix=a%2Fb"
        );
        assert_eq!(canonical_query(&[]), "");
    }

    #[test]
    fn the_date_is_utc_civil_time() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        // 2015-08-30 12:36:00 UTC, get-vanilla's instant.
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        // A leap day, and the last second of a year.
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
        assert_eq!(amz_date(1_798_761_599), "20261231T235959Z");
    }

    /// Every day from 1970 to past 2500, against a calendar walked one day at a time. The
    /// century terms of the closed form only differ from a wrong one near 2100, 2200, 2300
    /// and 2400, so four sample dates in this century cannot pin them (M286's mutants did).
    #[test]
    fn every_day_to_2500_matches_a_calendar_walk() {
        let leap =
            |y: u64| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
        let (mut y, mut m, mut d) = (1970_u64, 1_u64, 1_u64);
        for day in 0..194_000_u64 {
            assert_eq!(
                amz_date(day * 86_400 + 3_661),
                format!("{y:04}{m:02}{d:02}T010101Z"),
                "day {day}"
            );
            let len = match m {
                2 if leap(y) => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            d += 1;
            if d > len {
                d = 1;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
        }
        assert_eq!(y, 2501);
    }

    #[test]
    fn a_session_token_never_reaches_debug() {
        let c = Credentials {
            key_id: "id".into(),
            secret: "s3cret".into(),
            token: Some("t0ken".into()),
        };
        let d = format!("{c:?}");
        assert!(
            d.contains("id") && !d.contains("s3cret") && !d.contains("t0ken"),
            "{d}"
        );
    }

    #[test]
    fn the_empty_hash_is_the_empty_bodys() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
    }
}
