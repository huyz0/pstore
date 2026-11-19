//! pstore's own Azure Blob client (M288): a [`BlobStore`] over M286's `hyper` client,
//! signed in-crate with SharedKey or a bearer token.
//!
//! ⚠️ **No retries in here**, for M286's reason: `Congested` is the one retry layer, and a
//! conditional write repeated after it landed answers `Lost` for a write that won (M282).
//!
//! ⚠️ **Errors come from the status code**: 404 absent, 412 `Lost`, 409 `BlobAlreadyExists`
//! `Lost` and any other 409 `Contended`, 503 or `ServerBusy` throttled, 416 out of range.
//!
//! ⚠️ **No suffix reads** (C-14): Azure's `x-ms-range` has no `bytes=-N` form, and Azurite
//! answers one with a 500. The capabilities say so, and a suffix read is refused here before
//! any request, as `object_store` refuses it.

pub mod sign;
pub mod token;

use crate::s3::creds::Cache;
use crate::s3::{Http, http_client, xml};
use crate::{BlobError, BlobStore, Capabilities, CasError, Key, Precondition, PutOutcome};
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use pstore_types::CasTag;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The service version every request names: `object_store` 0.14's.
pub const VERSION: &str = "2023-11-03";

/// A batch's limit, the Blob Batch API's.
pub const MAX_BATCH: usize = 256;

/// Where a container is.
#[derive(Debug, Clone)]
pub struct AzureConfig {
    /// The storage account: the name SharedKey signs with.
    pub account: String,
    /// The container.
    pub container: String,
    /// The account's root, with no trailing slash: `https://{account}.blob.core.windows.net`,
    /// or Azurite's `http://127.0.0.1:10000/devstoreaccount1`.
    pub endpoint: String,
    /// One request's whole budget, as [`crate::s3::S3Config::timeout`].
    pub timeout: std::time::Duration,
}

/// How requests are authorized.
#[derive(Debug, Clone)]
pub enum AzureAuth {
    /// The account key, base64 as Azure hands it out: SharedKey.
    Key(String),
    /// A bearer token from a provider.
    Token(token::Provider),
}

#[derive(Debug, Clone)]
enum Signer {
    Key(Arc<Vec<u8>>),
    Token(Arc<Cache<token::Provider>>),
}

/// The client.
#[derive(Debug, Clone)]
pub struct AzureBackend {
    http: Http,
    caps: Capabilities,
    scheme: String,
    authority: String,
    /// The path before the container: empty on Azure, `/devstoreaccount1` on Azurite.
    base: String,
    account: String,
    container: String,
    signer: Signer,
    timeout: std::time::Duration,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl AzureBackend {
    /// A client for `config`, with `caps` from the conformance suite, and what Azure is
    /// whatever the profile says: no suffix reads, batches of [`MAX_BATCH`], deletes billed
    /// (as `ObjectStoreBackend::azure`).
    ///
    /// # Errors
    /// An endpoint that is not `http(s)://host[/path]`, a key that is not base64, or a system
    /// with no TLS roots.
    pub fn new(
        config: AzureConfig,
        auth: AzureAuth,
        caps: Capabilities,
    ) -> Result<Self, BlobError> {
        let (scheme, rest) = config
            .endpoint
            .trim_end_matches('/')
            .split_once("://")
            .filter(|(s, a)| matches!(*s, "http" | "https") && !a.is_empty())
            .ok_or_else(|| {
                BlobError::Other(format!(
                    "endpoint {:?} is not scheme://host[/path]",
                    config.endpoint
                ))
            })?;
        let (authority, base) = match rest.split_once('/') {
            Some((a, p)) => (a.to_owned(), format!("/{p}")),
            None => (rest.to_owned(), String::new()),
        };
        let http = http_client()?;
        let signer = match auth {
            AzureAuth::Key(k) => Signer::Key(Arc::new(sign::decode_key(&k)?)),
            AzureAuth::Token(p) => Signer::Token(Arc::new(Cache::new(p, http.clone()))),
        };
        Ok(Self {
            http,
            caps: Capabilities {
                suffix_read: false,
                max_batch_delete: MAX_BATCH,
                delete_is_free: false,
                ..caps
            },
            scheme: scheme.to_owned(),
            authority,
            base,
            account: config.account,
            container: config.container,
            signer,
            timeout: config.timeout,
        })
    }

    fn container_path(&self) -> String {
        format!(
            "{}/{}",
            self.base,
            crate::s3::sign::uri_encode(&self.container, false)
        )
    }

    fn path(&self, key: &Key) -> String {
        format!(
            "{}/{}",
            self.container_path(),
            crate::s3::sign::uri_encode(key.as_str(), true)
        )
    }

    /// The `Authorization` value for one request, whose headers already include
    /// `x-ms-date` and `x-ms-version`.
    async fn authorization(&self, r: &sign::Request<'_>) -> Result<String, BlobError> {
        Ok(match &self.signer {
            Signer::Key(k) => sign::authorization(&self.account, k, r),
            Signer::Token(cache) => format!("Bearer {}", cache.get().await?),
        })
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
        let date = sign::http_date(crate::s3::creds::now());
        let len = body.len().to_string();
        let mut headers: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 3);
        headers.push(("x-ms-date", &date));
        headers.push(("x-ms-version", VERSION));
        // Every request carries its length: a zero is signed as empty, and Azure needs
        // one on a body-less PUT.
        headers.push(("content-length", &len));
        headers.extend_from_slice(extra);
        let auth = self
            .authorization(&sign::Request {
                method: method.as_str(),
                path,
                query,
                headers: &headers,
            })
            .await?;
        let q = crate::s3::sign::canonical_query(query);
        let uri = if q.is_empty() {
            format!("{}://{}{path}", self.scheme, self.authority)
        } else {
            format!("{}://{}{path}?{q}", self.scheme, self.authority)
        };
        let mut req = http::Request::builder().method(method).uri(uri);
        for (k, v) in &headers {
            req = req.header(*k, *v);
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
                .map_err(|e| BlobError::Other(format!("azure: {e}")))?;
            let (parts, body) = resp.into_parts();
            let body = body
                .collect()
                .await
                .map_err(|e| BlobError::Other(format!("azure body: {e}")))?
                .to_bytes();
            Ok(Reply {
                status: parts.status,
                headers: parts.headers,
                body,
            })
        };
        tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| BlobError::Other(format!("azure: no answer within {:?}", self.timeout)))?
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

    /// One Blob Batch request deleting every key, each sub-request signed on its own.
    async fn batch_delete(&self, keys: &[Key]) -> Result<(), BlobError> {
        static BATCHES: AtomicU64 = AtomicU64::new(0);
        let boundary = format!(
            "batch_{}_{}",
            crate::s3::creds::now(),
            BATCHES.fetch_add(1, Ordering::Relaxed)
        );
        let date = sign::http_date(crate::s3::creds::now());
        let mut body = String::with_capacity(keys.len() * 320);
        for (i, key) in keys.iter().enumerate() {
            let path = self.path(key);
            let headers = [
                ("x-ms-date", date.as_str()),
                ("x-ms-version", VERSION),
                ("content-length", "0"),
            ];
            let auth = self
                .authorization(&sign::Request {
                    method: "DELETE",
                    path: &path,
                    query: &[],
                    headers: &headers,
                })
                .await?;
            body.push_str(&format!(
                "--{boundary}\r\nContent-Type: application/http\r\n\
                 Content-Transfer-Encoding: binary\r\nContent-ID: {i}\r\n\r\n\
                 DELETE {path} HTTP/1.1\r\n"
            ));
            for (k, v) in headers {
                body.push_str(&format!("{k}: {v}\r\n"));
            }
            body.push_str(&format!("authorization: {auth}\r\n\r\n\r\n"));
        }
        body.push_str(&format!("--{boundary}--\r\n"));
        let content_type = format!("multipart/mixed; boundary={boundary}");
        let r = self
            .send(
                Method::POST,
                &self.container_path(),
                &[("restype", "container"), ("comp", "batch")],
                &[("content-type", &content_type)],
                Bytes::from(body),
            )
            .await?;
        if r.status != StatusCode::ACCEPTED {
            return Err(status_err(&r, &Key::new("?comp=batch")));
        }
        match batch_failures(&r.headers, &r.body, keys)?
            .into_iter()
            .next()
        {
            None => Ok(()),
            Some((key, status)) => Err(BlobError::Other(format!(
                "delete of {} failed: {status}",
                key.as_str()
            ))),
        }
    }

    /// A ranged read of `range`, which is not empty.
    async fn ranged(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        let header = format!("bytes={}-{}", range.start, range.end - 1);
        let r = self
            .send_key(Method::GET, key, &[("x-ms-range", &header)], Bytes::new())
            .await?;
        match r.status {
            StatusCode::PARTIAL_CONTENT => Ok(r.body),
            // A server that ignores the range sends the whole object.
            StatusCode::OK => {
                let len = r.body.len() as u64;
                if range.end > len {
                    return Err(BlobError::RangeOutOfBounds(range, len));
                }
                Ok(r.body.slice(range.start as usize..range.end as usize))
            }
            StatusCode::RANGE_NOT_SATISFIABLE => {
                let len = content_range_total(&r.headers).unwrap_or(0);
                Err(BlobError::RangeOutOfBounds(range, len))
            }
            _ => Err(status_err(&r, key)),
        }
    }
}

/// The keys a batch's `202` reports as failed, with their sub-status. A 404 is not a
/// failure: a delete is idempotent.
///
/// # Errors
/// A response that is not the multipart answer, or a failed part with no `Content-ID`,
/// which is the batch itself refused.
fn batch_failures(
    headers: &HeaderMap,
    body: &[u8],
    keys: &[Key],
) -> Result<Vec<(Key, u16)>, BlobError> {
    let bad = |why: &str| BlobError::Other(format!("azure batch: {why}"));
    let boundary = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once("boundary="))
        .map(|(_, b)| b.trim_matches('"').to_owned())
        .ok_or_else(|| bad("no multipart boundary"))?;
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    let mut answered = 0usize;
    for part in text.split(&format!("--{boundary}")).skip(1) {
        if part.starts_with("--") {
            break;
        }
        let mut id = None;
        let mut status = None;
        for line in part.lines() {
            let line = line.trim();
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-id")
            {
                id = v.trim().parse::<usize>().ok();
            }
            if status.is_none()
                && let Some(rest) = line.strip_prefix("HTTP/1.1 ")
            {
                status = rest.get(..3).and_then(|c| c.parse::<u16>().ok());
            }
        }
        let status = status.ok_or_else(|| bad("a part with no status line"))?;
        answered += 1;
        if (200..300).contains(&status) || status == 404 {
            continue;
        }
        let key = id
            .and_then(|i| keys.get(i))
            .ok_or_else(|| bad(&format!("refused with {status}")))?;
        out.push((key.clone(), status));
    }
    if answered != keys.len() {
        return Err(bad(&format!(
            "{answered} answers for {} deletes",
            keys.len()
        )));
    }
    Ok(out)
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
    // Absent is empty, never invented, as on S3.
    CasTag::new(
        h.get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
    )
}

/// `x-ms-error-code`, which every error carries in a header (a HEAD has no body).
fn error_code(h: &HeaderMap) -> &str {
    h.get("x-ms-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

fn status_err(r: &Reply, key: &Key) -> BlobError {
    let code = error_code(&r.headers);
    match r.status {
        StatusCode::NOT_FOUND => BlobError::NotFound(key.as_str().to_owned()),
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS => BlobError::SlowDown,
        _ if code == "ServerBusy" => BlobError::SlowDown,
        s => BlobError::Other(format!("azure {s} on {}: {code}", key.as_str())),
    }
}

#[async_trait::async_trait]
impl BlobStore for AzureBackend {
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
            return Ok(Bytes::new());
        }
        let got = self.ranged(key, range.clone()).await?;
        // A `206` may carry fewer bytes than asked for when the range overruns the object.
        if got.len() as u64 != range.end - range.start {
            return Err(BlobError::RangeOutOfBounds(range, got.len() as u64));
        }
        Ok(got)
    }

    async fn get_suffix(&self, _key: &Key, _n: u64) -> Result<Bytes, BlobError> {
        // `object_store`'s words, so the capability matrix's two Azure rows read alike.
        Err(BlobError::Other(
            "Operation not supported: Azure does not support suffix range requests".to_owned(),
        ))
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
        let r = self
            .send_key(Method::PUT, key, &[("x-ms-blob-type", "BlockBlob")], body)
            .await?;
        if r.status != StatusCode::CREATED {
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
            .send_key(
                Method::PUT,
                key,
                &[("x-ms-blob-type", "BlockBlob"), cond],
                body,
            )
            .await
            .map_err(|e| CasError::Io(e.to_string()))?;
        match r.status {
            StatusCode::CREATED => Ok(PutOutcome {
                tag: etag(&r.headers),
            }),
            StatusCode::PRECONDITION_FAILED => Err(CasError::Lost),
            // `If-None-Match: *` on a blob that exists.
            StatusCode::CONFLICT if error_code(&r.headers) == "BlobAlreadyExists" => {
                Err(CasError::Lost)
            }
            // An `If-Match` on a blob deleted since: another writer won (M287's finding).
            StatusCode::NOT_FOUND if matches!(pre, Precondition::Match(_)) => Err(CasError::Lost),
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
        match keys {
            [] => Ok(()),
            // One key is one request either way; a plain DELETE needs no multipart.
            [key] => {
                let r = self
                    .send_key(Method::DELETE, key, &[], Bytes::new())
                    .await?;
                match r.status {
                    StatusCode::ACCEPTED | StatusCode::NOT_FOUND => Ok(()),
                    _ => Err(status_err(&r, key)),
                }
            }
            _ => self.batch_delete(keys).await,
        }
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        // The blobs directly under `prefix/`, every page, as the S3 client answers.
        let dir = if prefix.as_str().is_empty() {
            String::new()
        } else {
            format!("{}/", prefix.as_str().trim_end_matches('/'))
        };
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        let path = self.container_path();
        loop {
            let mut query = vec![
                ("restype", "container"),
                ("comp", "list"),
                ("delimiter", "/"),
                ("prefix", dir.as_str()),
            ];
            if let Some(m) = &marker {
                query.push(("marker", m.as_str()));
            }
            let r = self
                .send(Method::GET, &path, &query, &[], Bytes::new())
                .await?;
            if r.status != StatusCode::OK {
                return Err(status_err(&r, prefix));
            }
            out.extend(list_names(&r.body).into_iter().map(Key::new));
            let next = xml::text(&r.body, "NextMarker").filter(|m| !m.is_empty());
            if next.is_some() && next == marker {
                return Err(BlobError::Other(
                    "azure list: the marker did not advance".to_owned(),
                ));
            }
            marker = next;
            if marker.is_none() {
                return Ok(out);
            }
        }
    }
}

/// A List Blobs page's blob names: each `<Blob>`'s `<Name>`, not the `<BlobPrefix>`es.
fn list_names(body: &[u8]) -> Vec<String> {
    let Ok(s) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    xml::raw_elements(s, "Blob")
        .into_iter()
        .flat_map(|b| xml::all(b, "Name").into_iter().take(1))
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn multipart(boundary: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "content-type",
            format!("multipart/mixed; boundary={boundary}")
                .parse()
                .unwrap(),
        );
        h
    }

    #[test]
    fn a_batch_answer_names_the_keys_that_failed() {
        let keys = [Key::new("a"), Key::new("b"), Key::new("c")];
        let body = "--r\r\nContent-Type: application/http\r\nContent-ID: 0\r\n\r\n\
                    HTTP/1.1 202 Accepted\r\nx-ms-delete-type-permanent: true\r\n\r\n\
                    --r\r\nContent-Type: application/http\r\nContent-ID: 1\r\n\r\n\
                    HTTP/1.1 404 The specified blob does not exist.\r\n\r\n\
                    --r\r\nContent-Type: application/http\r\nContent-ID: 2\r\n\r\n\
                    HTTP/1.1 403 Forbidden\r\n\r\n--r--\r\n";
        assert_eq!(
            batch_failures(&multipart("r"), body.as_bytes(), &keys).unwrap(),
            [(Key::new("c"), 403)]
        );
        // Fewer answers than deletes is not a success.
        let short = "--r\r\nContent-ID: 0\r\n\r\nHTTP/1.1 202 Accepted\r\n\r\n--r--\r\n";
        assert!(batch_failures(&multipart("r"), short.as_bytes(), &keys).is_err());
        // A failure with no Content-ID is the whole batch refused.
        let refused = "--r\r\n\r\nHTTP/1.1 400 Bad Request\r\n\r\n--r--\r\n";
        assert!(batch_failures(&multipart("r"), refused.as_bytes(), &keys[..1]).is_err());
        assert!(batch_failures(&HeaderMap::new(), body.as_bytes(), &keys).is_err());
        let no_status = "--r\r\nContent-ID: 0\r\n\r\n\r\n--r--\r\n";
        assert!(batch_failures(&multipart("r"), no_status.as_bytes(), &keys[..1]).is_err());
    }

    #[test]
    fn a_listing_names_blobs_not_prefixes() {
        let page = br#"<?xml version="1.0"?><EnumerationResults><Blobs><BlobPrefix><Name>t/d/</Name></BlobPrefix><Blob><Name>t/a&amp;b</Name><Properties><Content-Length>1</Content-Length></Properties></Blob><Blob><Name>t/c</Name></Blob></Blobs><NextMarker /></EnumerationResults>"#;
        assert_eq!(list_names(page), ["t/a&b", "t/c"]);
        assert_eq!(xml::text(page, "NextMarker"), None);
        assert!(list_names(&[0xff, 0xfe]).is_empty());
    }

    #[test]
    fn statuses_and_error_codes_are_typed() {
        let reply = |status: u16, code: &str| {
            let mut headers = HeaderMap::new();
            if !code.is_empty() {
                headers.insert("x-ms-error-code", code.parse().unwrap());
            }
            Reply {
                status: StatusCode::from_u16(status).unwrap(),
                headers,
                body: Bytes::new(),
            }
        };
        let k = Key::new("k");
        assert!(matches!(
            status_err(&reply(404, "BlobNotFound"), &k),
            BlobError::NotFound(_)
        ));
        assert_eq!(status_err(&reply(503, ""), &k), BlobError::SlowDown);
        assert_eq!(status_err(&reply(429, ""), &k), BlobError::SlowDown);
        assert_eq!(
            status_err(&reply(500, "ServerBusy"), &k),
            BlobError::SlowDown
        );
        assert_eq!(
            status_err(&reply(500, "InternalError"), &k),
            BlobError::Other("azure 500 Internal Server Error on k: InternalError".to_owned())
        );
    }
}
