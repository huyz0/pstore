//! Azure Storage's SharedKey (M288), computed in-crate with `ring`.
//!
//! <https://learn.microsoft.com/en-us/rest/api/storageservices/authorize-with-shared-key>,
//! the Blob service's form (version 2015-02-21 and later): twelve standard headers in a fixed
//! order, then the `x-ms-*` headers sorted, then the account and the URI path, then the query.

use crate::BlobError;

/// One request to sign. Header names are matched without case; `path` is the URI path as
/// sent (percent-encoded); `query` holds names and values **decoded**.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// `GET`, `PUT`, ...
    pub method: &'a str,
    /// `/container/key`, or `/devstoreaccount1/container/key` on Azurite.
    pub path: &'a str,
    /// The query parameters, decoded.
    pub query: &'a [(&'a str, &'a str)],
    /// Every header sent.
    pub headers: &'a [(&'a str, &'a str)],
}

/// The standard headers the string-to-sign holds, in its order.
const STANDARD: [&str; 11] = [
    "content-encoding",
    "content-language",
    "content-length",
    "content-md5",
    "content-type",
    "date",
    "if-modified-since",
    "if-match",
    "if-none-match",
    "if-unmodified-since",
    "range",
];

/// The string-to-sign for `account`.
#[must_use]
pub fn string_to_sign(account: &str, r: &Request<'_>) -> String {
    let header = |name: &str| {
        r.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map_or("", |(_, v)| *v)
    };
    let mut out = String::with_capacity(256);
    out.push_str(r.method);
    out.push('\n');
    for name in STANDARD {
        let v = header(name);
        // ⚠️ A zero length is signed as empty, from version 2015-02-21.
        if !(name == "content-length" && v == "0") {
            out.push_str(v);
        }
        out.push('\n');
    }
    let mut ms: Vec<(String, &str)> = r
        .headers
        .iter()
        .filter(|(k, _)| k.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("x-ms-")))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim()))
        .collect();
    ms.sort_unstable();
    for (k, v) in ms {
        out.push_str(&k);
        out.push(':');
        out.push_str(v);
        out.push('\n');
    }
    out.push('/');
    out.push_str(account);
    out.push_str(r.path);
    let mut names: Vec<String> = r
        .query
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let mut values: Vec<&str> = r
            .query
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
            .map(|(_, v)| *v)
            .collect();
        values.sort_unstable();
        out.push('\n');
        out.push_str(&name);
        out.push(':');
        out.push_str(&values.join(","));
    }
    out
}

/// An account key as Azure hands it out: base64.
///
/// # Errors
/// A key that is not base64.
pub fn decode_key(key: &str) -> Result<Vec<u8>, BlobError> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(key.trim())
        .map_err(|e| BlobError::Other(format!("the Azure account key is not base64: {e}")))
}

/// The `Authorization` header's value: `SharedKey account:signature`.
#[must_use]
pub fn authorization(account: &str, key: &[u8], r: &Request<'_>) -> String {
    use base64::Engine as _;
    let mac = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key),
        string_to_sign(account, r).as_bytes(),
    );
    format!(
        "SharedKey {account}:{}",
        base64::engine::general_purpose::STANDARD.encode(mac.as_ref())
    )
}

/// `unix` as the RFC 1123 date Azure's `x-ms-date` takes: `Tue, 17 Nov 2026 00:00:00 GMT`.
#[must_use]
pub fn http_date(unix: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // `20261117T000000Z`, which the S3 signer already derives and tests for every day.
    let a = crate::s3::sign::amz_date(unix);
    let month: usize = a.get(4..6).and_then(|m| m.parse().ok()).unwrap_or(1);
    format!(
        "{}, {} {} {} {}:{}:{} GMT",
        DAYS.get(usize::try_from((unix / 86_400) % 7).unwrap_or(0))
            .copied()
            .unwrap_or_default(),
        a.get(6..8).unwrap_or_default(),
        MONTHS
            .get(month.saturating_sub(1))
            .copied()
            .unwrap_or_default(),
        a.get(0..4).unwrap_or_default(),
        a.get(9..11).unwrap_or_default(),
        a.get(11..13).unwrap_or_default(),
        a.get(13..15).unwrap_or_default(),
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// Azure's documented layout, field by field, for a Put Blob with a condition and a
    /// List Blobs query with a repeated parameter.
    #[test]
    fn the_string_to_sign_is_azures_layout() {
        let put = Request {
            method: "PUT",
            path: "/c/a%20b",
            query: &[],
            headers: &[
                ("Content-Length", "4"),
                ("If-Match", "\"0x1\""),
                ("x-ms-version", "2023-11-03"),
                ("X-Ms-Date", "Tue, 17 Nov 2026 00:00:00 GMT"),
                ("x-ms-blob-type", " BlockBlob "),
                ("content-type", "application/octet-stream"),
            ],
        };
        assert_eq!(
            string_to_sign("acct", &put),
            "PUT\n\n\n4\n\napplication/octet-stream\n\n\n\"0x1\"\n\n\n\n\
             x-ms-blob-type:BlockBlob\nx-ms-date:Tue, 17 Nov 2026 00:00:00 GMT\n\
             x-ms-version:2023-11-03\n/acct/c/a%20b"
        );
        let list = Request {
            method: "GET",
            path: "/c",
            query: &[
                ("restype", "container"),
                ("comp", "list"),
                ("Prefix", "t/x"),
                ("include", "b"),
                ("include", "a"),
            ],
            headers: &[("content-length", "0"), ("range", "bytes=0-9")],
        };
        assert_eq!(
            string_to_sign("acct", &list),
            "GET\n\n\n\n\n\n\n\n\n\n\nbytes=0-9\n/acct/c\ncomp:list\ninclude:a,b\nprefix:t/x\n\
             restype:container"
        );
    }

    /// The signature of a known string with a known key, computed independently:
    /// `printf '%s' "$STS" | openssl dgst -sha256 -mac HMAC -macopt hexkey:6b6579 -binary | base64`.
    #[test]
    fn shared_key_is_hmac_sha256_of_the_string_to_sign() {
        let r = Request {
            method: "GET",
            path: "/c/k",
            query: &[],
            headers: &[("x-ms-date", "Tue, 17 Nov 2026 00:00:00 GMT")],
        };
        assert_eq!(
            authorization("acct", b"key", &r),
            "SharedKey acct:wuhPV4/BuWT6j2UNRQuyxqczIecjG4W9Zzk5fnV8Zls="
        );
        assert_eq!(decode_key("a2V5").unwrap(), b"key");
        assert!(decode_key("not base64!").is_err());
    }

    #[test]
    fn http_dates_name_the_right_day() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_795_089_600), "Thu, 19 Nov 2026 12:00:00 GMT");
        assert_eq!(
            http_date(951_782_400 + 3_661),
            "Tue, 29 Feb 2000 01:01:01 GMT"
        );
        assert_eq!(http_date(1_794_873_600), "Tue, 17 Nov 2026 00:00:00 GMT");
        // Every weekday and month name once, so none of the tables can be shuffled.
        let mut seen = std::collections::BTreeSet::new();
        for day in 0..400_u64 {
            let d = http_date(day * 86_400);
            seen.insert(d[..3].to_owned());
            seen.insert(d[8..11].to_owned());
        }
        assert_eq!(seen.len(), 19);
    }
}
