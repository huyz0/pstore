//! pstore's own S3 client (M286): a [`BlobStore`] over `hyper`, signed in-crate.
//!
//! ⚠️ **No retries in here.** `Congested` is the one retry layer, and it can only adapt to a
//! `503` it sees. `object_store` retries inside the client, up to ten times, before the
//! caller hears of one, and repeats a conditional write that landed and then got a status,
//! which M282 found answers `Lost` for a write that won.
//!
//! ⚠️ **Errors come from the status code**, never from a message: 404 absent, 412 `Lost`,
//! 409 `Contended`, 503 throttled, 416 out of range.

pub mod sign;
mod xml;

use crate::{BlobError, BlobStore, Capabilities, CasError, Key, Precondition, PutOutcome};
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use pstore_types::CasTag;
use sign::{Credentials, EMPTY_SHA256};
use std::ops::Range;

type Https = hyper_rustls::HttpsConnector<HttpConnector>;

/// Where a bucket is, and who is asking.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// `https://s3.us-east-1.amazonaws.com`, or an emulator's `http://127.0.0.1:9000`.
    pub endpoint: String,
    /// The bucket.
    pub bucket: String,
    /// The signing region.
    pub region: String,
    /// The signing credentials.
    pub credentials: Credentials,
    /// `bucket.host/key` rather than `host/bucket/key`. AWS prefers it; emulators mostly
    /// serve only the path style.
    pub virtual_hosted: bool,
    /// One request's whole budget, connect to last body byte. ⚠️ Without one a stalled
    /// connection holds its caller forever; `object_store`'s default is 30 s.
    pub timeout: std::time::Duration,
}

impl S3Config {
    /// Credentials from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
    ///
    /// # Errors
    /// When either of the first two is unset. ⚠️ The instance-profile, container and
    /// web-identity providers are not here (M286 scope): their absence is a refusal, never an
    /// anonymous request.
    pub fn credentials_from_env() -> Result<Credentials, BlobError> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        match (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
            (Some(key_id), Some(secret)) => Ok(Credentials {
                key_id,
                secret,
                token: var("AWS_SESSION_TOKEN"),
            }),
            _ => Err(BlobError::Other(
                "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must both be set".to_owned(),
            )),
        }
    }
}

/// The client.
#[derive(Debug, Clone)]
pub struct S3Backend {
    http: Client<Https, Full<Bytes>>,
    caps: Capabilities,
    scheme: String,
    /// The `host` header: `host[:port]`, with the bucket in front when virtual-hosted.
    host: String,
    /// The path before the key: `/bucket` path-style, empty virtual-hosted.
    base: String,
    region: String,
    credentials: Credentials,
    timeout: std::time::Duration,
}

/// What came back from one request.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl S3Backend {
    /// A profile for a bucket the conformance suite has not run against: divergent, so a
    /// `durable` write is refused rather than trusted (as `ObjectStoreBackend::unprobed`).
    #[must_use]
    pub fn unprobed(backend: impl Into<String>) -> Capabilities {
        Capabilities {
            backend: backend.into(),
            cas: crate::Support::Divergent("not yet probed by the conformance suite".to_owned()),
            create_if_absent: crate::Support::Divergent("not yet probed".to_owned()),
            delete_is_free: false,
            max_batch_delete: 1000,
            coalesce_gap: 1024 * 1024,
            suffix_read: true,
        }
    }

    /// A client for `config`, with `caps` from the conformance suite.
    ///
    /// # Errors
    /// An endpoint that is not `http://` or `https://` and a host, or a system with no TLS
    /// roots.
    pub fn new(config: S3Config, caps: Capabilities) -> Result<Self, BlobError> {
        let (scheme, authority) = config
            .endpoint
            .trim_end_matches('/')
            .split_once("://")
            .filter(|(s, a)| matches!(*s, "http" | "https") && !a.is_empty() && !a.contains('/'))
            .ok_or_else(|| {
                BlobError::Other(format!(
                    "endpoint {:?} is not scheme://host[:port]",
                    config.endpoint
                ))
            })?;
        let (host, base) = if config.virtual_hosted {
            (format!("{}.{authority}", config.bucket), String::new())
        } else {
            (
                authority.to_owned(),
                format!("/{}", sign::uri_encode(&config.bucket, false)),
            )
        };
        let tls = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|e| BlobError::Other(format!("no TLS roots: {e}")))?
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            http: Client::builder(TokioExecutor::new()).build(tls),
            caps,
            scheme: scheme.to_owned(),
            host,
            base,
            region: config.region,
            credentials: config.credentials,
            timeout: config.timeout,
        })
    }

    fn path(&self, key: &Key) -> String {
        format!("{}/{}", self.base, sign::uri_encode(key.as_str(), true))
    }

    /// Signs and sends one request. `extra` headers are signed too.
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        extra: &[(&str, &str)],
        body: Bytes,
    ) -> Result<Reply, BlobError> {
        let payload = if body.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            sign::sha256_hex(&body)
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let date = sign::amz_date(now);
        let mut headers: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 4);
        headers.push(("host", &self.host));
        headers.push(("x-amz-content-sha256", &payload));
        headers.push(("x-amz-date", &date));
        if let Some(t) = &self.credentials.token {
            headers.push(("x-amz-security-token", t));
        }
        headers.extend_from_slice(extra);
        let auth = sign::authorization(
            &self.credentials,
            &sign::Scope {
                amz_date: &date,
                region: &self.region,
                service: "s3",
            },
            &sign::Request {
                method: method.as_str(),
                path,
                query,
                headers: &headers,
                payload_sha256: &payload,
            },
        );
        let q = sign::canonical_query(query);
        let uri = if q.is_empty() {
            format!("{}://{}{path}", self.scheme, self.host)
        } else {
            format!("{}://{}{path}?{q}", self.scheme, self.host)
        };
        let mut req = http::Request::builder().method(method).uri(uri);
        for (k, v) in &headers {
            // `host` is the URI's; hyper writes it.
            if *k != "host" {
                req = req.header(*k, *v);
            }
        }
        let req = req
            .header("authorization", auth)
            .body(Full::new(body))
            .map_err(|e| BlobError::Other(e.to_string()))?;
        let exchange = async {
            let resp = self
                .http
                .request(req)
                .await
                .map_err(|e| BlobError::Other(format!("s3: {e}")))?;
            let (parts, body) = resp.into_parts();
            // ⚠️ `to_bytes` on a one-frame body hands that frame on, no copy; on several it
            // allocates the total once.
            let body = body
                .collect()
                .await
                .map_err(|e| BlobError::Other(format!("s3 body: {e}")))?
                .to_bytes();
            Ok(Reply {
                status: parts.status,
                headers: parts.headers,
                body,
            })
        };
        tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| BlobError::Other(format!("s3: no answer within {:?}", self.timeout)))?
    }

    async fn send_key(
        &self,
        method: Method,
        key: &Key,
        extra: &[(&str, &str)],
        body: Bytes,
    ) -> Result<Reply, BlobError> {
        self.send(method, &self.path(key), &[], extra, body).await
    }

    /// A ranged read: `range`, or with `None` the last `suffix` bytes.
    async fn ranged(
        &self,
        key: &Key,
        header: &str,
        range: Option<Range<u64>>,
        suffix: u64,
    ) -> Result<Bytes, BlobError> {
        let r = self
            .send_key(Method::GET, key, &[("range", header)], Bytes::new())
            .await?;
        match r.status {
            StatusCode::PARTIAL_CONTENT => Ok(r.body),
            // A server that ignores `Range` sends the whole object.
            StatusCode::OK => match range {
                Some(range) => {
                    let len = r.body.len() as u64;
                    if range.end > len {
                        return Err(BlobError::RangeOutOfBounds(range, len));
                    }
                    Ok(r.body.slice(range.start as usize..range.end as usize))
                }
                // A suffix: the last `n` bytes of what came back.
                None => {
                    let skip = r
                        .body
                        .len()
                        .saturating_sub(usize::try_from(suffix).unwrap_or(usize::MAX));
                    Ok(r.body.slice(skip..))
                }
            },
            StatusCode::RANGE_NOT_SATISFIABLE => {
                let len = content_range_total(&r.headers).unwrap_or(0);
                Err(BlobError::RangeOutOfBounds(range.unwrap_or(0..0), len))
            }
            _ => Err(status_err(&r, key)),
        }
    }
}

/// `Content-Range: bytes */N` or `bytes a-b/N`: `N`.
fn content_range_total(h: &HeaderMap) -> Option<u64> {
    h.get("content-range")?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

fn etag(h: &HeaderMap) -> CasTag {
    // ⚠️ Absent is empty, never invented: an empty tag cannot condition a write, so a backend
    // that sends none is refused at the CAS rather than appearing to fence.
    CasTag::new(
        h.get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
    )
}

fn status_err(r: &Reply, key: &Key) -> BlobError {
    match r.status {
        StatusCode::NOT_FOUND => BlobError::NotFound(key.as_str().to_owned()),
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS => BlobError::SlowDown,
        s => BlobError::Other(format!(
            "s3 {s} on {}: {}",
            key.as_str(),
            xml::text(&r.body, "Code").unwrap_or_default()
        )),
    }
}

#[async_trait::async_trait]
impl BlobStore for S3Backend {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        Ok(self.get_with_tag(key).await?.0)
    }

    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        if range.start > range.end {
            return Err(BlobError::RangeOutOfBounds(range, 0));
        }
        if range.start == range.end {
            // HTTP cannot say "zero bytes at N"; the trait's answer is the empty slice.
            return Ok(Bytes::new());
        }
        let header = format!("bytes={}-{}", range.start, range.end - 1);
        let got = self.ranged(key, &header, Some(range.clone()), 0).await?;
        // ⚠️ A `206` may carry fewer bytes than asked for when the range overruns the object:
        // a short read, which the trait makes an error (the conformance suite's finding).
        if got.len() as u64 != range.end - range.start {
            return Err(BlobError::RangeOutOfBounds(range, got.len() as u64));
        }
        Ok(got)
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        if n == 0 {
            return Ok(Bytes::new());
        }
        self.ranged(key, &format!("bytes=-{n}"), None, n).await
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        let r = self.send_key(Method::GET, key, &[], Bytes::new()).await?;
        if r.status != StatusCode::OK {
            return Err(status_err(&r, key));
        }
        Ok((r.body, etag(&r.headers)))
    }

    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        match self.stat(key).await {
            Ok((_, tag)) => Ok(tag),
            Err(BlobError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        Ok(self.stat(key).await?.0)
    }

    async fn stat(&self, key: &Key) -> Result<(u64, Option<CasTag>), BlobError> {
        let r = self.send_key(Method::HEAD, key, &[], Bytes::new()).await?;
        if r.status != StatusCode::OK {
            return Err(status_err(&r, key));
        }
        let len = r
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| BlobError::Other("HEAD without a Content-Length".to_owned()))?;
        let tag = r.headers.contains_key("etag").then(|| etag(&r.headers));
        Ok((len, tag))
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        let r = self.send_key(Method::PUT, key, &[], body).await?;
        if r.status != StatusCode::OK {
            return Err(status_err(&r, key));
        }
        Ok(PutOutcome {
            tag: etag(&r.headers),
        })
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        let cond = match &pre {
            Precondition::NotExists => ("if-none-match", "*"),
            Precondition::Match(tag) => ("if-match", tag.as_str()),
        };
        let r = self
            .send_key(Method::PUT, key, &[cond], body)
            .await
            .map_err(|e| CasError::Io(e.to_string()))?;
        match r.status {
            StatusCode::OK => Ok(PutOutcome {
                tag: etag(&r.headers),
            }),
            StatusCode::PRECONDITION_FAILED => Err(CasError::Lost),
            // S3 answers `If-None-Match: *` on a key that exists with 412, and a conditional
            // write racing another on the same key with 409 `ConditionalRequestConflict`.
            StatusCode::CONFLICT => Err(CasError::Contended),
            _ => Err(CasError::Io(status_err(&r, key).to_string())),
        }
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        if keys.len() > self.caps.max_batch_delete {
            return Err(BlobError::Other(format!(
                "batch of {} exceeds the backend's limit of {}",
                keys.len(),
                self.caps.max_batch_delete
            )));
        }
        if keys.is_empty() {
            return Ok(());
        }
        let body = Bytes::from(xml::delete_request(keys));
        let md5 = {
            use base64::Engine as _;
            use md5::Digest as _;
            base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(&body))
        };
        let r = self
            .send(
                Method::POST,
                &format!("{}/", self.base),
                &[("delete", "")],
                &[("content-md5", &md5), ("content-type", "application/xml")],
                body,
            )
            .await?;
        if r.status != StatusCode::OK {
            return Err(status_err(&r, &Key::new("?delete")));
        }
        // ⚠️ A 200 can still name keys that failed. Quiet mode lists only those; a missing
        // key is not one of them on S3, which deletes idempotently.
        match xml::delete_errors(&r.body).first() {
            None => Ok(()),
            Some((key, code)) => Err(BlobError::Other(format!("delete of {key} failed: {code}"))),
        }
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        // The keys directly under `prefix/`, every page: what `object_store`'s
        // `list_with_delimiter` answers, which this replaces.
        let dir = if prefix.as_str().is_empty() {
            String::new()
        } else {
            format!("{}/", prefix.as_str().trim_end_matches('/'))
        };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        let path = format!("{}/", self.base);
        loop {
            let mut query = vec![
                ("list-type", "2"),
                ("delimiter", "/"),
                ("prefix", dir.as_str()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token", t.as_str()));
            }
            let r = self
                .send(Method::GET, &path, &query, &[], Bytes::new())
                .await?;
            if r.status != StatusCode::OK {
                return Err(status_err(&r, prefix));
            }
            out.extend(xml::list_keys(&r.body).into_iter().map(Key::new));
            token = xml::text(&r.body, "NextContinuationToken");
            if token.is_none() {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn config(endpoint: &str, virtual_hosted: bool) -> S3Config {
        S3Config {
            endpoint: endpoint.to_owned(),
            bucket: "b k".to_owned(),
            region: "us-east-1".to_owned(),
            credentials: Credentials {
                key_id: "k".into(),
                secret: "s".into(),
                token: None,
            },
            virtual_hosted,
            timeout: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn the_endpoint_must_be_a_scheme_and_a_host() {
        let caps = S3Backend::unprobed("s3");
        for bad in ["127.0.0.1:9000", "ftp://h", "http://", "http://h/path"] {
            assert!(
                S3Backend::new(config(bad, false), caps.clone()).is_err(),
                "{bad}"
            );
        }
        let s = S3Backend::new(config("http://h:9/", false), caps.clone()).unwrap();
        assert_eq!((s.host.as_str(), s.base.as_str()), ("h:9", "/b%20k"));
        assert_eq!(s.path(&Key::new("a/b c")), "/b%20k/a/b%20c");
        let v = S3Backend::new(config("https://h", true), caps).unwrap();
        assert_eq!((v.host.as_str(), v.base.as_str()), ("b k.h", ""));
        assert_eq!(v.path(&Key::new("x")), "/x");
    }

    #[test]
    fn a_content_range_names_the_objects_length() {
        let mut h = HeaderMap::new();
        assert_eq!(content_range_total(&h), None);
        h.insert("content-range", "bytes */42".parse().unwrap());
        assert_eq!(content_range_total(&h), Some(42));
        h.insert("content-range", "bytes 0-9/7".parse().unwrap());
        assert_eq!(content_range_total(&h), Some(7));
    }
}
