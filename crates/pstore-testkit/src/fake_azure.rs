//! A fake Azure Blob service on loopback (M288): enough of the REST API for pstore's
//! `BlobStore`, over real HTTP, laid out as Azurite is (`/{account}/{container}/{blob}`).
//!
//! ⚠️ **It checks every request's SharedKey** with `pstore_blob::azure::sign`, Blob Batch
//! sub-requests included. Both clients talk to it, so `object_store`'s requests passing is
//! evidence that pstore's signer computes what Azure's does (M288 criterion 2).
//!
//! What it is not: Azure. One account, one container, block blobs only, no leases, no
//! versions. ETags are a counter, never content-derived, as Azure's are.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Response, StatusCode};
use pstore_blob::azure::sign;
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The one account: Azurite's.
pub const ACCOUNT: &str = "devstoreaccount1";
/// Its key, base64: `fake-azure-key`.
pub const KEY: &str = "ZmFrZS1henVyZS1rZXk=";
/// The one container.
pub const CONTAINER: &str = "pstore";

/// An answer the next request gets instead of its own.
#[derive(Debug, Clone)]
pub struct Fault {
    /// The status.
    pub status: u16,
    /// The `x-ms-error-code`.
    pub code: &'static str,
    /// Apply the write anyway before answering: a PUT that landed and lost its reply.
    pub apply: bool,
}

#[derive(Default)]
struct State {
    blobs: BTreeMap<String, (Bytes, String)>,
    requests: Vec<(Method, String)>,
    faults: VecDeque<Fault>,
    bad_signatures: u64,
    next_tag: u64,
    page: Option<std::num::NonZeroUsize>,
    bearer: Option<String>,
    ignore_ranges: bool,
}

/// A running fake. Dropping it stops the listener.
pub struct FakeAzure {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeAzure {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock(s: &Mutex<State>) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeAzure {
    /// Listens on any free port.
    ///
    /// # Errors
    /// The bind's.
    pub async fn start() -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
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

    /// The account's root: `http://127.0.0.1:port/devstoreaccount1`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}/{ACCOUNT}", self.addr)
    }

    /// Requests served so far, as `(method, path?query)`; a batch's sub-requests are not
    /// requests.
    #[must_use]
    pub fn requests(&self) -> Vec<(Method, String)> {
        lock(&self.state).requests.clone()
    }

    /// Requests (and batch sub-requests) whose authorization did not verify.
    #[must_use]
    pub fn bad_signatures(&self) -> u64 {
        lock(&self.state).bad_signatures
    }

    /// The next request gets `fault`.
    pub fn fail_next(&self, fault: Fault) {
        lock(&self.state).faults.push_back(fault);
    }

    /// Blobs a list page holds (5000 by default, as Azure).
    pub fn set_page(&self, n: usize) {
        lock(&self.state).page = std::num::NonZeroUsize::new(n.max(1));
    }

    /// Accept `Authorization: Bearer {token}` as well as SharedKey.
    pub fn accept_bearer(&self, token: &str) {
        lock(&self.state).bearer = Some(token.to_owned());
    }

    /// Answer every read with the whole blob and a `200`, as a server that ignores
    /// `x-ms-range` does.
    pub fn ignore_ranges(&self) {
        lock(&self.state).ignore_ranges = true;
    }

    /// The pstore client's configuration for this fake.
    #[must_use]
    pub fn config(&self) -> pstore_blob::azure::AzureConfig {
        pstore_blob::azure::AzureConfig {
            account: ACCOUNT.to_owned(),
            container: CONTAINER.to_owned(),
            endpoint: self.endpoint(),
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
        &[
            ("x-ms-error-code", code.to_owned()),
            ("content-type", "application/xml".to_owned()),
        ],
        Bytes::from(format!("<Error><Code>{code}</Code></Error>")),
    )
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut rest = b.iter();
    while let Some(&c) = rest.next() {
        let hex = rest
            .as_slice()
            .get(..2)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (c, hex) {
            (b'%', Some(v)) => {
                out.push(v);
                rest.nth(1);
            }
            _ => out.push(c),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn query(uri: &http::Uri) -> Vec<(String, String)> {
    uri.query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// Whether `auth` is what SharedKey computes for this request, or the accepted bearer token.
fn verified(
    bearer: Option<&str>,
    method: &str,
    path: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
) -> bool {
    let Some(auth) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.as_str())
    else {
        return false;
    };
    if let Some(token) = auth.strip_prefix("Bearer ") {
        return bearer == Some(token);
    }
    let Ok(key) = sign::decode_key(KEY) else {
        return false;
    };
    let q: Vec<(&str, &str)> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let h: Vec<(&str, &str)> = headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("authorization"))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    sign::authorization(
        ACCOUNT,
        &key,
        &sign::Request {
            method,
            path,
            query: &q,
            headers: &h,
        },
    ) == auth
}

fn header_pairs(h: &http::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect()
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
    let q = query(&parts.uri);
    let headers = header_pairs(&parts.headers);
    let mut st = lock(state);
    st.requests.push((
        parts.method.clone(),
        parts
            .uri
            .path_and_query()
            .map(ToString::to_string)
            .unwrap_or_default(),
    ));
    if !verified(
        st.bearer.as_deref(),
        parts.method.as_str(),
        parts.uri.path(),
        &q,
        &headers,
    ) {
        st.bad_signatures += 1;
        return error(StatusCode::FORBIDDEN, "AuthenticationFailed");
    }
    let path = decode(parts.uri.path());
    let Some(rest) = path.strip_prefix(&format!("/{ACCOUNT}/{CONTAINER}")) else {
        return error(StatusCode::NOT_FOUND, "ContainerNotFound");
    };
    let key = rest.strip_prefix('/').unwrap_or(rest).to_owned();
    if let Some(f) = st.faults.pop_front() {
        if f.apply && parts.method == Method::PUT {
            let tag = new_tag(&mut st);
            st.blobs.insert(key, (body, tag));
        }
        return error(
            StatusCode::from_u16(f.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            f.code,
        );
    }
    let header = |k: &str| {
        parts
            .headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let param = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    match (parts.method.clone(), key.is_empty()) {
        // The container's two operations: pstore sends no other.
        (Method::POST, true) => batch(&mut st, header("content-type").unwrap_or_default(), &body),
        (Method::GET, true) => list(&st, &param("prefix").unwrap_or_default(), param("marker")),
        (Method::PUT, false) => {
            if header("x-ms-blob-type").as_deref() != Some("BlockBlob") {
                return error(StatusCode::BAD_REQUEST, "MissingRequiredHeader");
            }
            let current = st.blobs.get(&key).map(|(_, t)| t.clone());
            match (header("if-none-match").as_deref(), header("if-match")) {
                (Some("*"), _) if current.is_some() => {
                    return error(StatusCode::CONFLICT, "BlobAlreadyExists");
                }
                // A missing blob too, as Azurite answers, which `object_store` reads as
                // `Lost` (the capability matrix's `cas_after_delete_is_lost`).
                (_, Some(want)) if current.as_deref() != Some(want.as_str()) => {
                    return error(StatusCode::PRECONDITION_FAILED, "ConditionNotMet");
                }
                _ => {}
            }
            let tag = new_tag(&mut st);
            st.blobs.insert(key, (body, tag.clone()));
            reply(
                StatusCode::CREATED,
                &[("etag", tag), ("last-modified", MODIFIED.to_owned())],
                Bytes::new(),
            )
        }
        (Method::GET | Method::HEAD, false) => {
            let Some((data, tag)) = st.blobs.get(&key).cloned() else {
                return error(StatusCode::NOT_FOUND, "BlobNotFound");
            };
            let len = data.len() as u64;
            let mut out = vec![
                ("etag", tag),
                ("last-modified", MODIFIED.to_owned()),
                ("x-ms-blob-type", "BlockBlob".to_owned()),
            ];
            let range = header("x-ms-range").or_else(|| header("range"));
            if parts.method == Method::HEAD {
                out.push(("content-length", len.to_string()));
                return reply(StatusCode::OK, &out, Bytes::new());
            }
            let spec = range.as_deref().and_then(|r| r.strip_prefix("bytes="));
            let Some(spec) = spec.filter(|_| !st.ignore_ranges) else {
                return reply(StatusCode::OK, &out, data);
            };
            // Only `bytes=a-b`, which is all pstore sends.
            let parsed = spec
                .split_once('-')
                .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
            let Some((start, end)) = parsed else {
                // `bytes=-N`: Azure has no suffix form, and Azurite answers it with a 500.
                return error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError");
            };
            if start >= len {
                out.push(("content-range", format!("bytes */{len}")));
                return reply(StatusCode::RANGE_NOT_SATISFIABLE, &out, Bytes::new());
            }
            let end = end.min(len - 1);
            out.push(("content-range", format!("bytes {start}-{end}/{len}")));
            reply(
                StatusCode::PARTIAL_CONTENT,
                &out,
                data.slice(start as usize..=end as usize),
            )
        }
        (Method::DELETE, false) => match st.blobs.remove(&key) {
            Some(_) => reply(StatusCode::ACCEPTED, &[], Bytes::new()),
            None => error(StatusCode::NOT_FOUND, "BlobNotFound"),
        },
        _ => error(StatusCode::BAD_REQUEST, "UnsupportedHttpVerb"),
    }
}

fn new_tag(st: &mut State) -> String {
    st.next_tag += 1;
    format!("\"0x8DE{:012X}\"", st.next_tag)
}

/// List Blobs, `delimiter=/`, one page.
fn list(st: &State, prefix: &str, marker: Option<String>) -> Response<Full<Bytes>> {
    let page = st.page.map_or(5000, std::num::NonZeroUsize::get);
    let mut names = Vec::new();
    let mut prefixes = std::collections::BTreeSet::new();
    let mut next = None;
    // The marker is where the page starts: the first name the last page left out.
    let from = marker.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Included);
    for (name, (data, tag)) in st.blobs.range((from, std::ops::Bound::Unbounded)) {
        if !name.starts_with(prefix) {
            continue;
        }
        if names.len() + prefixes.len() == page {
            next = Some(name.clone());
            break;
        }
        match name[prefix.len()..].find('/') {
            Some(i) => {
                prefixes.insert(name[..prefix.len() + i + 1].to_owned());
            }
            None => names.push((name.clone(), data.len(), tag.clone())),
        }
    }
    let mut out =
        String::from(r#"<?xml version="1.0" encoding="utf-8"?><EnumerationResults><Blobs>"#);
    for p in &prefixes {
        out.push_str(&format!(
            "<BlobPrefix><Name>{}</Name></BlobPrefix>",
            escape(p)
        ));
    }
    for (n, len, tag) in &names {
        out.push_str(&format!(
            "<Blob><Name>{}</Name><Properties><Last-Modified>{MODIFIED}</Last-Modified>\
             <Etag>{}</Etag><Content-Length>{}</Content-Length>\
             <Content-Type>application/octet-stream</Content-Type><BlobType>BlockBlob</BlobType>\
             </Properties></Blob>",
            escape(n),
            escape(tag),
            len
        ));
    }
    out.push_str("</Blobs>");
    match next {
        Some(m) => out.push_str(&format!("<NextMarker>{}</NextMarker>", escape(&m))),
        None => out.push_str("<NextMarker />"),
    }
    out.push_str("</EnumerationResults>");
    reply(
        StatusCode::OK,
        &[("content-type", "application/xml".to_owned())],
        Bytes::from(out),
    )
}

/// Blob Batch: every part a signed `DELETE`, answered part by part, `Content-ID` for
/// `Content-ID`.
fn batch(st: &mut State, content_type: String, body: &[u8]) -> Response<Full<Bytes>> {
    let Some(boundary) = content_type
        .strip_prefix("multipart/mixed; boundary=")
        .map(str::to_owned)
    else {
        return error(StatusCode::BAD_REQUEST, "InvalidHeaderValue");
    };
    let text = String::from_utf8_lossy(body).into_owned();
    let mut parts = Vec::new();
    for part in text.split(&format!("--{boundary}")).skip(1) {
        if part.starts_with("--") {
            break;
        }
        parts.push(part.to_owned());
    }
    if parts.is_empty() || parts.len() > 256 {
        return error(StatusCode::BAD_REQUEST, "InvalidInput");
    }
    let out_boundary = "batchresponse_fake";
    let mut out = String::new();
    for part in parts {
        let (head, request) = part
            .trim_start_matches("\r\n")
            .split_once("\r\n\r\n")
            .unwrap_or_default();
        let id = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-id")
                .then(|| v.trim().to_owned())
        });
        let mut lines = request.split("\r\n");
        let line = lines.next().unwrap_or_default();
        let mut words = line.split(' ');
        let (method, path) = (
            words.next().unwrap_or_default(),
            words.next().unwrap_or_default(),
        );
        let headers: Vec<(String, String)> = lines
            .take_while(|l| !l.is_empty())
            .filter_map(|l| {
                let (k, v) = l.split_once(':')?;
                Some((k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            })
            .collect();
        let status = if method != "DELETE" {
            "400 Bad Request"
        } else if !verified(st.bearer.as_deref(), method, path, &[], &headers) {
            st.bad_signatures += 1;
            "403 Server failed to authenticate the request."
        } else {
            let decoded = decode(path);
            let key = decoded
                .strip_prefix(&format!("/{ACCOUNT}/{CONTAINER}/"))
                .unwrap_or_default();
            match st.blobs.remove(key) {
                Some(_) => "202 Accepted",
                None => "404 The specified blob does not exist.",
            }
        };
        out.push_str(&format!(
            "--{out_boundary}\r\nContent-Type: application/http\r\n"
        ));
        if let Some(id) = id {
            out.push_str(&format!("Content-ID: {id}\r\n"));
        }
        out.push_str(&format!(
            "\r\nHTTP/1.1 {status}\r\nx-ms-version: 2023-11-03\r\n\r\n"
        ));
    }
    out.push_str(&format!("--{out_boundary}--\r\n"));
    reply(
        StatusCode::ACCEPTED,
        &[(
            "content-type",
            format!("multipart/mixed; boundary={out_boundary}"),
        )],
        Bytes::from(out),
    )
}
