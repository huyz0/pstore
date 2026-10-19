//! GCS's batch endpoint (M107): one request deletes up to 100 objects.
//!
//! `object_store` 0.14.1 deletes on GCS one request per object, because the JSON API it
//! speaks has no bulk delete. Its batch endpoint does: `POST /batch/storage/v1` carries up to
//! 100 sub-requests as `multipart/mixed`, and answers each in its own part. The encoder and
//! the parser are free functions so the wire format is pinned without an emulator.

use crate::{BlobError, Key};
use object_store::client::{HttpClient, HttpRequestBody};
use object_store::gcp::GcpCredentialProvider;

/// The API's cap on sub-requests in one batch.
pub(crate) const MAX_BATCH: usize = 100;

/// ⚠️ **Holds `=`, which no encoded name can.** [`encode`] escapes everything outside the
/// unreserved set, so the delimiter cannot occur inside a part, whatever the keys are.
const BOUNDARY: &str = "pstore==batch";

/// Percent-encodes `name` for a path segment: `/` is `%2F`, since every key pstore writes
/// has one, and GCS would otherwise read the rest as a path.
pub(crate) fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The batch's `Content-Type` and body: one `DELETE` part per key, in order.
pub(crate) fn request(bucket: &str, keys: &[Key]) -> (String, String) {
    let mut body = String::new();
    for (i, k) in keys.iter().enumerate() {
        // ⚠️ The blank line after the request line ends the sub-request's headers, and the
        // CRLF after it belongs to the delimiter, not to the part. Without both, the
        // emulator answers every part `400`: the part ends inside its headers.
        body.push_str(&format!(
            "--{BOUNDARY}\r\nContent-Type: application/http\r\nContent-ID: <{n}>\r\n\r\n\
             DELETE /storage/v1/b/{b}/o/{o} HTTP/1.1\r\n\r\n\r\n",
            n = i + 1,
            b = encode(bucket),
            o = encode(k.as_str()),
        ));
    }
    body.push_str(&format!("--{BOUNDARY}--\r\n"));
    (format!("multipart/mixed; boundary=\"{BOUNDARY}\""), body)
}

/// Reads a batch's answer part by part. A `2xx` or a `404` is a delete: absent is deleted,
/// as it is on `MemoryStore`. Anything else fails the batch, naming the key.
pub(crate) fn parse(content_type: &str, body: &str, keys: &[Key]) -> Result<(), BlobError> {
    let boundary = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"'))
        .filter(|b| !b.is_empty())
        .ok_or_else(|| {
            BlobError::Other(format!("batch answer is not multipart: {content_type}"))
        })?;
    let delimiter = format!("--{boundary}");
    let mut statuses = Vec::new();
    for (i, part) in body.split(delimiter.as_str()).skip(1).enumerate() {
        if part.starts_with("--") {
            break;
        }
        let status = part
            .lines()
            .find_map(|l| l.strip_prefix("HTTP/"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| BlobError::Other(format!("batch part {} has no status line", i + 1)))?;
        statuses.push(status);
    }
    if statuses.len() != keys.len() {
        return Err(BlobError::Other(format!(
            "batch answered {} parts for {} keys",
            statuses.len(),
            keys.len()
        )));
    }
    for (k, s) in keys.iter().zip(statuses) {
        if !((200..300).contains(&s) || s == 404) {
            return Err(BlobError::Other(format!(
                "batch delete of {} answered {s}",
                k.as_str()
            )));
        }
    }
    Ok(())
}

/// What a GCS backend needs to send a batch: its client, its credentials, and where.
#[derive(Debug)]
pub(crate) struct GcsBatch {
    pub(crate) client: HttpClient,
    pub(crate) credentials: GcpCredentialProvider,
    pub(crate) endpoint: String,
    pub(crate) bucket: String,
}

impl GcsBatch {
    /// One request for `keys`, which the caller has already capped.
    ///
    /// ⚠️ **No retry here.** A failed batch fails `delete_batch`, and GC's next pass names
    /// the same keys again, since the graveyard entry is pruned only after a success.
    pub(crate) async fn delete(&self, keys: &[Key]) -> Result<(), BlobError> {
        let cred = self
            .credentials
            .get_credential()
            .await
            .map_err(|e| BlobError::Other(e.to_string()))?;
        let (content_type, body) = request(&self.bucket, keys);
        let mut req = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!(
                "{}/batch/storage/v1",
                self.endpoint.trim_end_matches('/')
            ))
            .header(http::header::CONTENT_TYPE, content_type);
        // The emulator's `disable_oauth` credential is empty, and an empty bearer is a
        // malformed header rather than an absent one.
        if !cred.bearer.is_empty() {
            req = req.header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", cred.bearer),
            );
        }
        let req = req
            .body(HttpRequestBody::from(body))
            .map_err(|e| BlobError::Other(e.to_string()))?;
        let res = self
            .client
            .execute(req)
            .await
            .map_err(|e| BlobError::Other(e.to_string()))?;
        let status = res.status();
        let content_type = res
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = res
            .into_body()
            .bytes()
            .await
            .map_err(|e| BlobError::Other(e.to_string()))?;
        if !status.is_success() {
            return Err(if matches!(status.as_u16(), 429 | 503) {
                BlobError::SlowDown
            } else {
                BlobError::Other(format!("batch delete answered {status}"))
            });
        }
        parse(&content_type, &String::from_utf8_lossy(&bytes), keys)
    }
}

/// An in-process GCS that records each request and gives one canned answer, so the request
/// path is checked by `cargo test` and the mutation sweep, which never start an emulator.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
pub(crate) mod mock {
    use object_store::client::{
        HttpError, HttpRequest, HttpResponse, HttpResponseBody, HttpService,
    };
    use std::sync::{Arc, Mutex};

    /// A request as sent: method, URI, `Content-Type`, `Authorization`, body.
    pub(crate) type Sent = (String, String, String, Option<String>, String);

    #[derive(Debug, Clone)]
    pub(crate) struct Gcs {
        pub(crate) sent: Arc<Mutex<Vec<Sent>>>,
        status: u16,
        content_type: String,
        body: String,
    }

    impl Gcs {
        pub(crate) fn answering(status: u16, content_type: &str, body: &str) -> Self {
            Self {
                sent: Arc::default(),
                status,
                content_type: content_type.to_owned(),
                body: body.to_owned(),
            }
        }

        pub(crate) fn sent(&self) -> Vec<Sent> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HttpService for Gcs {
        async fn call(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            let header = |h: http::header::HeaderName| {
                req.headers().get(h).map(|v| v.to_str().unwrap().to_owned())
            };
            self.sent.lock().unwrap().push((
                req.method().to_string(),
                req.uri().to_string(),
                header(http::header::CONTENT_TYPE).unwrap_or_default(),
                header(http::header::AUTHORIZATION),
                String::from_utf8(req.body().as_bytes().unwrap().to_vec()).unwrap(),
            ));
            Ok(http::Response::builder()
                .status(self.status)
                .header(http::header::CONTENT_TYPE, &self.content_type)
                .body(HttpResponseBody::from(self.body.clone()))
                .unwrap())
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;
    use object_store::StaticCredentialProvider;
    use object_store::gcp::GcpCredential;
    use std::sync::Arc;

    fn batch(gcs: &mock::Gcs, bearer: &str) -> GcsBatch {
        GcsBatch {
            client: HttpClient::new(gcs.clone()),
            credentials: Arc::new(StaticCredentialProvider::new(GcpCredential {
                bearer: bearer.to_owned(),
            })),
            endpoint: "http://gcs.invalid:1/".to_owned(),
            bucket: "pstore".to_owned(),
        }
    }

    fn keys(n: usize) -> Vec<Key> {
        (0..n).map(|i| Key::new(format!("tnt/7/seg/{i}"))).collect()
    }

    /// An answer in the emulator's shape, one part per status.
    fn answer(statuses: &[&str]) -> (String, String) {
        let b = "0088502f1b";
        let mut body = String::new();
        for (i, s) in statuses.iter().enumerate() {
            body.push_str(&format!(
                "--{b}\r\nContent-Id: <response-{}>\r\nContent-Type: application/http\r\n\r\n\
                 HTTP/1.1 {s}\r\nContent-Type: text/plain\r\n\r\nbody\r\n",
                i + 1
            ));
        }
        body.push_str(&format!("--{b}--\r\n"));
        (format!("multipart/mixed; boundary={b}"), body)
    }

    #[test]
    fn three_keys_are_three_delete_parts_and_a_closing_boundary() {
        let (ct, body) = request("pstore", &keys(3));
        assert_eq!(ct, "multipart/mixed; boundary=\"pstore==batch\"");
        assert_eq!(body.matches("--pstore==batch\r\n").count(), 3, "{body}");
        assert!(body.ends_with("--pstore==batch--\r\n"), "{body}");
        for i in 0..3 {
            assert!(
                body.contains(&format!(
                    "DELETE /storage/v1/b/pstore/o/tnt%2F7%2Fseg%2F{i} HTTP/1.1\r\n\r\n\r\n"
                )),
                "part {i}: {body}"
            );
            assert_eq!(body.matches(&format!("Content-ID: <{}>", i + 1)).count(), 1);
        }
    }

    #[test]
    fn a_name_is_encoded_outside_the_unreserved_set() {
        assert_eq!(
            encode("a/b c+d%=é~-._Z9"),
            "a%2Fb%20c%2Bd%25%3D%C3%A9~-._Z9"
        );
        // The boundary cannot occur in an encoded name: `=` is always escaped.
        assert!(!encode("x==y").contains('='));
    }

    #[test]
    fn a_2xx_or_a_404_is_a_delete() {
        let (ct, body) = answer(&["200 OK", "204 No Content", "404 Not Found"]);
        assert_eq!(parse(&ct, &body, &keys(3)), Ok(()));
    }

    #[test]
    fn a_403_in_the_second_part_fails_naming_the_second_key() {
        let (ct, body) = answer(&["200 OK", "403 Forbidden", "200 OK"]);
        let e = parse(&ct, &body, &keys(3)).unwrap_err().to_string();
        assert!(e.contains("tnt/7/seg/1") && e.contains("403"), "{e}");
        assert!(
            !e.contains("tnt/7/seg/0") && !e.contains("tnt/7/seg/2"),
            "{e}"
        );
    }

    #[test]
    fn two_answers_for_three_keys_fail() {
        let (ct, body) = answer(&["200 OK", "200 OK"]);
        let e = parse(&ct, &body, &keys(3)).unwrap_err().to_string();
        assert!(e.contains("2 parts for 3 keys"), "{e}");
    }

    #[test]
    fn a_part_with_no_status_line_fails() {
        let (ct, body) = answer(&["200 OK", "200 OK", "200 OK"]);
        let body = body.replacen("HTTP/1.1 200 OK", "garbage", 2);
        let e = parse(&ct, &body, &keys(3)).unwrap_err().to_string();
        assert!(e.contains("part 1 has no status line"), "{e}");
    }

    #[test]
    fn an_answer_that_is_not_multipart_fails() {
        assert!(parse("application/json", "{}", &keys(1)).is_err());
        let (_, body) = answer(&["200 OK"]);
        assert!(parse("multipart/mixed; boundary=", &body, &keys(1)).is_err());
    }

    #[tokio::test]
    async fn a_batch_is_one_post_with_the_bearer_and_the_encoded_body() {
        let (ct, body) = answer(&["204 No Content", "404 Not Found"]);
        let gcs = mock::Gcs::answering(200, &ct, &body);
        assert_eq!(batch(&gcs, "tok").delete(&keys(2)).await, Ok(()));
        let (want_ct, want_body) = request("pstore", &keys(2));
        assert_eq!(
            gcs.sent(),
            [(
                "POST".to_owned(),
                "http://gcs.invalid:1/batch/storage/v1".to_owned(),
                want_ct,
                Some("Bearer tok".to_owned()),
                want_body,
            )]
        );
    }

    #[tokio::test]
    async fn an_empty_bearer_sends_no_authorization() {
        let (ct, body) = answer(&["200 OK"]);
        let gcs = mock::Gcs::answering(200, &ct, &body);
        assert_eq!(batch(&gcs, "").delete(&keys(1)).await, Ok(()));
        assert_eq!(gcs.sent()[0].3, None);
    }

    #[tokio::test]
    async fn a_failed_part_fails_the_delete() {
        let (ct, body) = answer(&["200 OK", "403 Forbidden"]);
        let gcs = mock::Gcs::answering(200, &ct, &body);
        let e = batch(&gcs, "t").delete(&keys(2)).await.unwrap_err();
        assert!(e.to_string().contains("tnt/7/seg/1"), "{e}");
    }

    #[tokio::test]
    async fn a_throttled_batch_slows_down_and_any_other_failure_names_its_status() {
        for (status, slow) in [(429, true), (503, true), (500, false), (403, false)] {
            // The body is a valid answer, so only the batch's own status can fail it.
            let (ct, body) = answer(&["200 OK"]);
            let gcs = mock::Gcs::answering(status, &ct, &body);
            let e = batch(&gcs, "t").delete(&keys(1)).await.unwrap_err();
            if slow {
                assert_eq!(e, BlobError::SlowDown, "{status}");
            } else {
                assert!(
                    matches!(&e, BlobError::Other(m) if m.contains(&status.to_string())),
                    "{status}: {e:?}"
                );
            }
        }
    }
}
