//! The four XML shapes the client reads or writes, by hand: S3's answers are flat, and a
//! parser crate is a dependency for `<Key>` and `<Code>`.

use crate::Key;

/// `&`, `<`, `>`, `"`, `'` as entities.
fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
}

/// The five named entities and numeric references back to text.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or_default());
        let tail = rest.get(i..).unwrap_or_default();
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = tail.get(1..end).unwrap_or_default();
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| entity.strip_prefix('#').map(str::parse))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(tail.get(..=end).unwrap_or_default()),
        }
        rest = tail.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// Every `<tag>text</tag>` in `body`, in order, unescaped.
fn all(body: &str, tag: &str) -> Vec<String> {
    raw_elements(body, tag).into_iter().map(unescape).collect()
}

/// The first `<tag>`'s text.
pub(super) fn text(body: &[u8], tag: &str) -> Option<String> {
    all(std::str::from_utf8(body).ok()?, tag).into_iter().next()
}

/// A `ListObjectsV2` page's keys: each `<Contents>`'s `<Key>`, not the `<CommonPrefixes>`.
pub(super) fn list_keys(body: &[u8]) -> Vec<String> {
    let Ok(s) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    // The raw element, then its `<Key>` unescaped: unescaping first would let a key's
    // `&lt;Key&gt;` be read as a tag.
    raw_elements(s, "Contents")
        .into_iter()
        .flat_map(|c| all(c, "Key"))
        .collect()
}

/// Every `<tag>...</tag>`'s inner text, raw.
fn raw_elements<'a>(body: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(&open) {
        let after = rest.get(i + open.len()..).unwrap_or_default();
        let Some(j) = after.find(&close) else { break };
        out.push(after.get(..j).unwrap_or_default());
        rest = after.get(j + close.len()..).unwrap_or_default();
    }
    out
}

/// A quiet `DeleteObjects` request for `keys`.
pub(super) fn delete_request(keys: &[Key]) -> String {
    let mut out = String::with_capacity(64 + keys.len() * 48);
    out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?><Delete xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Quiet>true</Quiet>"#);
    for k in keys {
        out.push_str("<Object><Key>");
        escape(k.as_str(), &mut out);
        out.push_str("</Key></Object>");
    }
    out.push_str("</Delete>");
    out
}

/// A `DeleteResult`'s `<Error>`s, as `(key, code)`.
pub(super) fn delete_errors(body: &[u8]) -> Vec<(String, String)> {
    let Ok(s) = std::str::from_utf8(body) else {
        return vec![(String::new(), "an answer that is not UTF-8".to_owned())];
    };
    raw_elements(s, "Error")
        .into_iter()
        .map(|e| {
            let first = |t| all(e, t).into_iter().next().unwrap_or_default();
            (first("Key"), first("Code"))
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn a_key_survives_the_round_trip_through_xml() {
        let k = Key::new(r#"a&b<c>"d'e/é"#);
        let req = delete_request(std::slice::from_ref(&k));
        assert!(
            req.contains("<Key>a&amp;b&lt;c&gt;&quot;d&apos;e/é</Key>"),
            "{req}"
        );
        assert!(req.contains("<Quiet>true</Quiet>"));
        let page = format!(
            "<ListBucketResult><Contents><Key>{}</Key><Size>1</Size></Contents>\
            <CommonPrefixes><Prefix>p/</Prefix></CommonPrefixes>\
            <Contents><Key>plain</Key></Contents></ListBucketResult>",
            req.split("<Key>")
                .nth(1)
                .unwrap()
                .split("</Key>")
                .next()
                .unwrap()
        );
        assert_eq!(
            list_keys(page.as_bytes()),
            vec![k.as_str().to_owned(), "plain".to_owned()]
        );
    }

    #[test]
    fn numeric_and_unknown_entities() {
        assert_eq!(unescape("a&#65;&#x42;&bogus;&#xZZ;c"), "aAB&bogus;&#xZZ;c");
        assert_eq!(unescape("tail&amp"), "tail&amp");
        assert_eq!(unescape("none"), "none");
    }

    #[test]
    fn a_delete_result_names_what_failed() {
        let body = b"<DeleteResult><Error><Key>k&amp;1</Key><Code>AccessDenied</Code></Error>\
            <Error><Key>k2</Key><Code>InternalError</Code></Error></DeleteResult>";
        assert_eq!(
            delete_errors(body),
            vec![
                ("k&1".to_owned(), "AccessDenied".to_owned()),
                ("k2".to_owned(), "InternalError".to_owned())
            ]
        );
        assert!(delete_errors(b"<DeleteResult></DeleteResult>").is_empty());
        assert_eq!(delete_errors(&[0xff]).len(), 1);
    }

    #[test]
    fn the_first_tag_and_an_unclosed_one() {
        assert_eq!(
            text(b"<E><Code>SlowDown</Code><Code>x</Code></E>", "Code").as_deref(),
            Some("SlowDown")
        );
        assert_eq!(text(b"<Code>open", "Code"), None);
        assert_eq!(text(&[0xff], "Code"), None);
        assert!(list_keys(&[0xff]).is_empty());
        assert!(list_keys(b"<Contents><Key>x").is_empty());
    }
}
