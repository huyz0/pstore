//! A fake S3 on loopback (M286): enough of the REST API for pstore's `BlobStore`, over real
//! HTTP, so a client is exercised down to its bytes on the wire.
//!
//! ⚠️ **It checks every request's SigV4 signature** with `pstore_blob::s3::sign`. Both clients
//! talk to it, so `object_store`'s requests passing is evidence that pstore's signer computes
//! what AWS's does, beyond the published examples.
//!
//! What it is not: S3. Path-style only, one bucket, no versioning, no multipart, and an
//! `If-Match` on a missing key answers 412 where S3 answers 404.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Response, StatusCode};
use md5::Digest as _;
use pstore_blob::s3::sign;
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The one bucket.
pub const BUCKET: &str = "pstore";
/// The access key it accepts.
pub const KEY_ID: &str = "pstore-test";
/// Its secret.
pub const SECRET: &str = "pstore-test-secret/+=";
/// The signing region.
pub const REGION: &str = "us-east-1";

/// Writes under this prefix are acknowledged and dropped.
pub const DISCARD: &str = "discard/";

/// An answer the next request gets instead of its own.
#[derive(Debug, Clone)]
pub struct Fault {
    /// The status.
    pub status: u16,
    /// The `<Code>` in the error body.
    pub code: &'static str,
    /// Apply the write anyway before answering: a PUT that landed and lost its reply.
    pub apply: bool,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, (Bytes, String)>,
    requests: Vec<(Method, String)>,
    faults: VecDeque<Fault>,
    bad_signatures: u64,
    /// Keys a list page holds; `None` is S3's 1000.
    page: Option<std::num::NonZeroUsize>,
    ignore_ranges: bool,
    stuck_listing: bool,
}

/// A running fake. Dropping it stops the listener.
pub struct FakeS3 {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock(s: &Mutex<State>) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeS3 {
    /// Listens on `127.0.0.1:port` (0 for any).
    ///
    /// # Errors
    /// The bind's.
    pub async fn start_on(port: u16) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let _ = tcp.set_nodelay(true);
                let state = Arc::clone(&shared);
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(move |req| {
                        let state = Arc::clone(&state);
                        async move { Ok::<_, Infallible>(handle(&state, req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(tcp), svc)
                        .await;
                });
            }
        });
        Ok(Self { addr, state, task })
    }

    /// Listens on any free port.
    ///
    /// # Errors
    /// The bind's.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_on(0).await
    }

    /// `http://127.0.0.1:port`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Requests served so far, as `(method, path?query)`.
    #[must_use]
    pub fn requests(&self) -> Vec<(Method, String)> {
        lock(&self.state).requests.clone()
    }

    /// Requests whose signature did not verify.
    #[must_use]
    pub fn bad_signatures(&self) -> u64 {
        lock(&self.state).bad_signatures
    }

    /// The next request gets `fault`.
    pub fn fail_next(&self, fault: Fault) {
        lock(&self.state).faults.push_back(fault);
    }

    /// Keys a list page holds (1000 by default, as S3).
    pub fn set_page(&self, n: usize) {
        lock(&self.state).page = std::num::NonZeroUsize::new(n.max(1));
    }

    /// Answer a ranged GET with `200` and the whole object, as HTTP permits a server to.
    pub fn ignore_ranges(&self, on: bool) {
        lock(&self.state).ignore_ranges = on;
    }

    /// Ignore `continuation-token`, so every page is the first: a server looping.
    pub fn stuck_listing(&self, on: bool) {
        lock(&self.state).stuck_listing = on;
    }

    /// The pstore client's configuration for this fake.
    #[must_use]
    pub fn config(&self) -> pstore_blob::s3::S3Config {
        pstore_blob::s3::S3Config {
            endpoint: self.endpoint(),
            bucket: BUCKET.to_owned(),
            region: REGION.to_owned(),
            credentials: sign::Credentials {
                key_id: KEY_ID.to_owned(),
                secret: SECRET.to_owned(),
                token: None,
            },
            virtual_hosted: false,
            timeout: std::time::Duration::from_secs(30),
        }
    }
}

fn reply(status: StatusCode, headers: &[(&str, String)], body: Bytes) -> Response<Full<Bytes>> {
    let mut r = Response::builder().status(status);
    for (k, v) in headers {
        r = r.header(*k, v.as_str());
    }
    r.body(Full::new(body))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

fn error(status: StatusCode, code: &str) -> Response<Full<Bytes>> {
    reply(
        status,
        &[("content-type", "application/xml".to_owned())],
        Bytes::from(format!("<Error><Code>{code}</Code></Error>")),
    )
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (c, hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Whether `req`'s `Authorization` is what `sign` computes for it.
fn verified(parts: &http::request::Parts, body: &[u8]) -> bool {
    let header = |k: &str| parts.headers.get(k).and_then(|v| v.to_str().ok());
    let Some(auth) = header("authorization") else {
        return false;
    };
    let Some(signed) = auth
        .split(", ")
        .find_map(|p| p.strip_prefix("SignedHeaders="))
    else {
        return false;
    };
    let (Some(date), Some(payload)) = (header("x-amz-date"), header("x-amz-content-sha256")) else {
        return false;
    };
    if payload != "UNSIGNED-PAYLOAD" && payload != sign::sha256_hex(body) {
        return false;
    }
    let host = parts.uri.authority().map(ToString::to_string);
    let mut headers = Vec::new();
    for name in signed.split(';') {
        let v = if name == "host" {
            header("host").map(str::to_owned).or_else(|| host.clone())
        } else {
            header(name).map(str::to_owned)
        };
        let Some(v) = v else { return false };
        headers.push((name, v));
    }
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let raw: Vec<(String, String)> = parts
        .uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let query: Vec<(&str, &str)> = raw.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let path = sign::uri_encode(&decode(parts.uri.path()), true);
    let expected = sign::authorization(
        &sign::Credentials {
            key_id: KEY_ID.to_owned(),
            secret: SECRET.to_owned(),
            token: None,
        },
        &sign::Scope {
            amz_date: date,
            region: REGION,
            service: "s3",
        },
        &sign::Request {
            method: parts.method.as_str(),
            path: &path,
            query: &query,
            headers: &headers,
            payload_sha256: payload,
        },
    );
    expected == auth
}

fn query_param(uri: &http::Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (decode(k) == key).then(|| decode(v))
    })
}

const MODIFIED: &str = "Wed, 18 Nov 2026 00:00:00 GMT";

async fn handle(
    state: &Mutex<State>,
    req: Request<hyper::body::Incoming>,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return error(StatusCode::BAD_REQUEST, "IncompleteBody"),
    };
    // Before the lock: a 32 MiB body's hash is not the state's business.
    let signed = verified(&parts, &body);
    let mut st = lock(state);
    st.requests.push((
        parts.method.clone(),
        parts
            .uri
            .path_and_query()
            .map(ToString::to_string)
            .unwrap_or_default(),
    ));
    if !signed {
        st.bad_signatures += 1;
        return error(StatusCode::FORBIDDEN, "SignatureDoesNotMatch");
    }
    let path = decode(parts.uri.path());
    let Some(rest) = path.strip_prefix(&format!("/{BUCKET}")) else {
        return error(StatusCode::NOT_FOUND, "NoSuchBucket");
    };
    let key = rest.strip_prefix('/').unwrap_or(rest).to_owned();
    let fault = st.faults.pop_front();
    if let Some(f) = &fault {
        if f.apply && parts.method == Method::PUT {
            let tag = format!("\"{}\"", sign::hex(&md5::Md5::digest(&body)));
            st.objects.insert(key.clone(), (body.clone(), tag));
        }
        return error(
            StatusCode::from_u16(f.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            f.code,
        );
    }
    let header = |k: &str| parts.headers.get(k).and_then(|v| v.to_str().ok());
    match (parts.method.clone(), key.is_empty()) {
        (Method::POST, true) if query_param(&parts.uri, "delete").is_some() => {
            // As S3: quiet mode lists only failures (none here), verbose mode every key.
            let text = String::from_utf8_lossy(&body);
            let quiet = text.contains("<Quiet>true</Quiet>");
            let mut out = String::from("<DeleteResult>");
            for piece in text.split("<Key>").skip(1) {
                if let Some((k, _)) = piece.split_once("</Key>") {
                    st.objects.remove(&unescape(k));
                    if !quiet {
                        out.push_str(&format!("<Deleted><Key>{k}</Key></Deleted>"));
                    }
                }
            }
            out.push_str("</DeleteResult>");
            reply(StatusCode::OK, &[], Bytes::from(out))
        }
        (Method::GET, true) => list(&st, &parts.uri),
        (Method::PUT, false) => {
            let current = st.objects.get(&key).map(|(_, t)| t.clone());
            let refused = match (header("if-none-match"), header("if-match")) {
                (Some("*"), _) => current.is_some(),
                (_, Some(want)) => current.as_deref() != Some(want),
                _ => false,
            };
            if refused {
                return error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed");
            }
            let tag = format!("\"{}\"", sign::hex(&md5::Md5::digest(&body)));
            // A benchmark's writes: answered as stored, never kept, so a run of thousands
            // does not hold them all in memory.
            if !key.starts_with(DISCARD) {
                st.objects.insert(key, (body, tag.clone()));
            }
            reply(StatusCode::OK, &[("etag", tag)], Bytes::new())
        }
        (Method::GET | Method::HEAD, false) => {
            let Some((data, tag)) = st.objects.get(&key).cloned() else {
                return error(StatusCode::NOT_FOUND, "NoSuchKey");
            };
            let len = data.len() as u64;
            let mut headers = vec![
                ("etag", tag),
                ("last-modified", MODIFIED.to_owned()),
                ("accept-ranges", "bytes".to_owned()),
            ];
            if parts.method == Method::HEAD {
                headers.push(("content-length", len.to_string()));
                return reply(StatusCode::OK, &headers, Bytes::new());
            }
            let range = header("range").filter(|_| !st.ignore_ranges);
            let Some(range) = range.and_then(|r| r.strip_prefix("bytes=")) else {
                return reply(StatusCode::OK, &headers, data);
            };
            let (a, b) = range.split_once('-').unwrap_or((range, ""));
            let (start, end) = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
                (Some(s), Some(e)) => (s, e.saturating_add(1).min(len)),
                (Some(s), None) => (s, len),
                (None, Some(n)) => (len.saturating_sub(n), len),
                (None, None) => return error(StatusCode::BAD_REQUEST, "InvalidRange"),
            };
            if start >= len || start >= end {
                headers.push(("content-range", format!("bytes */{len}")));
                return reply(StatusCode::RANGE_NOT_SATISFIABLE, &headers, Bytes::new());
            }
            headers.push(("content-range", format!("bytes {start}-{}/{len}", end - 1)));
            #[allow(
                clippy::cast_possible_truncation,
                reason = "bounded by the object's length"
            )]
            let slice = data.slice(start as usize..end as usize);
            reply(StatusCode::PARTIAL_CONTENT, &headers, slice)
        }
        (Method::DELETE, false) => {
            st.objects.remove(&key);
            reply(StatusCode::NO_CONTENT, &[], Bytes::new())
        }
        _ => error(StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed"),
    }
}

fn list(st: &State, uri: &http::Uri) -> Response<Full<Bytes>> {
    let prefix = query_param(uri, "prefix").unwrap_or_default();
    let delimiter = query_param(uri, "delimiter");
    let after = query_param(uri, "continuation-token")
        .filter(|_| !st.stuck_listing)
        .unwrap_or_default();
    let mut keys = Vec::new();
    let mut next = None;
    for k in st.objects.keys() {
        if !k.starts_with(&prefix) || k.as_str() <= after.as_str() {
            continue;
        }
        if keys.len() == st.page.map_or(1000, std::num::NonZeroUsize::get) {
            next = keys.last().cloned();
            break;
        }
        // A key past the next delimiter is under a common prefix, which nothing reads:
        // left out, as S3 leaves it out of `Contents`.
        let tail = k.get(prefix.len()..).unwrap_or_default();
        if delimiter.as_deref().is_none_or(|d| !tail.contains(d)) {
            keys.push(k.clone());
        }
    }
    let mut out = String::from("<ListBucketResult>");
    for k in &keys {
        let len = st.objects.get(k).map_or(0, |(d, _)| d.len());
        out.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>2026-11-18T00:00:00.000Z</LastModified>\
             <ETag>&quot;x&quot;</ETag><Size>{len}</Size></Contents>",
            escape(k)
        ));
    }
    out.push_str(&format!("<KeyCount>{}</KeyCount>", keys.len()));
    match next {
        Some(t) => out.push_str(&format!(
            "<IsTruncated>true</IsTruncated><NextContinuationToken>{}</NextContinuationToken>",
            escape(&t)
        )),
        None => out.push_str("<IsTruncated>false</IsTruncated>"),
    }
    out.push_str("</ListBucketResult>");
    reply(
        StatusCode::OK,
        &[("content-type", "application/xml".to_owned())],
        Bytes::from(out),
    )
}
