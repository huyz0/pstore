//! M128: public request bodies, active requests and the body-read deadline are bounded.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use crate::support;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, Auth, IngressCounts, RequestLimits};
use pstore_types::LaneId;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use support::auth::{KEY, config, requests, token};
use tower::ServiceExt;

type Chunk = Result<bytes::Bytes, std::io::Error>;

fn private() -> Auth {
    Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).unwrap()
}

fn bearer() -> Auth {
    support::auth::auth(&config(&[("current", &KEY)], "current"))
}

fn limits(body: usize, active: usize, tenant: usize, pool: usize) -> RequestLimits {
    RequestLimits::builder()
        .body_bytes(body)
        .active_requests(active)
        .tenant_active_requests(tenant)
        .reserved_body_bytes(pool)
        .build()
        .unwrap()
}

fn serve(auth: Auth, limits: RequestLimits) -> (Arc<Api<MemoryStore>>, Accounted<MemoryStore>) {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::with_limits(store.clone(), LaneId(1), None, auth, limits).unwrap();
    assert_eq!(
        api.request_limits(),
        limits,
        "the Api admits under what it was given"
    );
    (api, store)
}

/// A valid write padded with trailing whitespace to exactly `len` bytes.
fn write_of(len: usize) -> Vec<u8> {
    let mut v = br#"{"documents":[{"id":"a","vector":[1]}]}"#.to_vec();
    assert!(v.len() <= len);
    v.resize(len, b' ');
    v
}

struct Answer {
    status: StatusCode,
    body: Value,
    retry_after: Option<String>,
}

async fn call(router: axum::Router, req: Request<Body>) -> Answer {
    let res = router.oneshot(req).await.unwrap();
    let status = res.status();
    let retry_after = res
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        retry_after,
    }
}

fn put(tenant: Option<&str>, bearer: Option<&str>) -> axum::http::request::Builder {
    let mut b = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json");
    if let Some(t) = tenant {
        b = b.header("x-pstore-tenant", t);
    }
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b
}

/// A body that records whether it was ever polled, and never yields.
fn spy(polled: &Arc<AtomicBool>) -> Body {
    let polled = Arc::clone(polled);
    Body::from_stream(futures_util::stream::poll_fn(move |_| {
        polled.store(true, Ordering::SeqCst);
        std::task::Poll::<Option<Chunk>>::Pending
    }))
}

/// A body whose chunks arrive when the test sends them; dropping the sender ends it.
fn channel() -> (tokio::sync::mpsc::UnboundedSender<Chunk>, Body) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Chunk>();
    let body = Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)));
    (tx, body)
}

/// Waits, without a timing threshold, until admission holds `want`.
async fn until(api: &Api<MemoryStore>, want: impl Fn(IngressCounts) -> bool) {
    for _ in 0..10_000 {
        if want(api.ingress_counts()) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "admission never reached the state: {:?}",
        api.ingress_counts()
    );
}

fn refused(a: &Answer, status: StatusCode, code: &str) {
    assert_eq!(a.status, status, "{}", a.body);
    assert_eq!(a.body["error"]["code"], code, "{}", a.body);
}

#[tokio::test]
async fn ingress_byte_limits_precede_engine_access() {
    const MAX: usize = 96;
    let (api, store) = serve(private(), limits(MAX, 4, 4, 4 * MAX));
    let at = write_of(MAX);
    let ok = call(
        api.clone().router(),
        put(Some("1"), None).body(Body::from(at.clone())).unwrap(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    let before = requests(&store);
    let over = write_of(MAX + 1);
    // With a length, without one, in one-byte fragments, and under a lying length.
    let fragments = |v: Vec<u8>| {
        Body::from_stream(futures_util::stream::iter(
            v.into_iter()
                .map(|b| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b]))),
        ))
    };
    for req in [
        put(Some("1"), None).body(Body::from(over.clone())).unwrap(),
        put(Some("1"), None).body(fragments(over.clone())).unwrap(),
        put(Some("1"), None)
            .header("content-length", "10")
            .body(fragments(over.clone()))
            .unwrap(),
    ] {
        let got = call(api.clone().router(), req).await;
        refused(&got, StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
    }
    // A declared length over the limit refuses without reading a byte.
    let polled = Arc::new(AtomicBool::new(false));
    let got = call(
        api.clone().router(),
        put(Some("1"), None)
            .header("content-length", (MAX + 1).to_string())
            .body(spy(&polled))
            .unwrap(),
    )
    .await;
    refused(&got, StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
    assert!(!polled.load(Ordering::SeqCst));
    // A body many times the limit is read no further than the limit's next chunk. It is
    // finite, so a broken limit reads it all and fails the assertions rather than hanging.
    let sent = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sent);
    let endless = Body::from_stream(futures_util::StreamExt::take(
        futures_util::stream::repeat_with(move || {
            counter.fetch_add(8, Ordering::SeqCst);
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"        "))
        }),
        MAX,
    ));
    let got = call(
        api.clone().router(),
        put(Some("1"), None).body(endless).unwrap(),
    )
    .await;
    refused(&got, StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
    assert!(sent.load(Ordering::SeqCst) <= MAX + 8, "{sent:?}");
    assert_eq!(requests(&store), before, "a refusal reached the store");
    assert_eq!(api.ingress_counts(), IngressCounts::default());
    // The same bytes, one fewer, are still a valid request.
    let ok = call(
        api.clone().router(),
        put(Some("1"), None).body(fragments(write_of(MAX))).unwrap(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn authorization_precedes_body_polling_and_admission() {
    let (api, store) = serve(bearer(), limits(1024, 2, 1, 2048));
    let before = requests(&store);
    let read_only = token("9", &["read"]);
    for (bearer, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("not-a-token"), StatusCode::UNAUTHORIZED),
        (Some(read_only.as_str()), StatusCode::FORBIDDEN),
    ] {
        let polled = Arc::new(AtomicBool::new(false));
        let got = call(
            api.clone().router(),
            put(None, bearer).body(spy(&polled)).unwrap(),
        )
        .await;
        assert_eq!(got.status, status, "{}", got.body);
        assert!(
            !polled.load(Ordering::SeqCst),
            "polled before authorization"
        );
        assert_eq!(api.ingress_counts(), IngressCounts::default());
    }
    // The verified tenant is the key, whatever the request claims: tenant 9 holds its one
    // slot, a second of its requests is refused, and tenant 10 is not charged for it.
    let nine = token("9", &["write"]);
    let (tx, body) = channel();
    let held = tokio::spawn(call(
        api.clone().router(),
        put(None, Some(&nine)).body(body).unwrap(),
    ));
    until(&api, |c| c.active == 1).await;
    let polled = Arc::new(AtomicBool::new(false));
    let got = call(
        api.clone().router(),
        put(None, Some(&nine)).body(spy(&polled)).unwrap(),
    )
    .await;
    refused(&got, StatusCode::TOO_MANY_REQUESTS, "overloaded");
    assert!(!polled.load(Ordering::SeqCst));
    assert_eq!(requests(&store), before, "a refusal reached the store");
    let ten = call(
        api.clone().router(),
        put(None, Some(&token("10", &["write"])))
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    assert_eq!(ten.status, StatusCode::OK, "{}", ten.body);
    drop(tx);
    let first = held.await.unwrap();
    assert_eq!(
        first.status,
        StatusCode::BAD_REQUEST,
        "an empty body is not JSON"
    );
    let ok = call(
        api.clone().router(),
        put(None, Some(&nine))
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn unsupported_encoding_never_reaches_json() {
    let (api, store) = serve(private(), limits(1024, 4, 4, 4096));
    for encoding in ["gzip", "br", "identity, deflate"] {
        let polled = Arc::new(AtomicBool::new(false));
        let got = call(
            api.clone().router(),
            put(Some("1"), None)
                .header("content-encoding", encoding)
                .body(spy(&polled))
                .unwrap(),
        )
        .await;
        refused(
            &got,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_content_encoding",
        );
        assert!(!polled.load(Ordering::SeqCst));
    }
    assert_eq!(requests(&store), [0; 4]);
    let ok = call(
        api.clone().router(),
        put(Some("1"), None)
            .header("content-encoding", "identity")
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn ingress_capacity_is_shared_and_tenant_scoped() {
    const BODY: usize = 128;
    let (api, _store) = serve(private(), limits(BODY, 3, 2, 3 * BODY));
    // Two routers of one Api: the limits are the Api's, not the router's.
    let (r1, r2) = (api.clone().router(), api.clone().router());
    let mut held = Vec::new();
    for (router, tenant) in [(&r1, "1"), (&r2, "1"), (&r2, "2")] {
        let (tx, body) = channel();
        held.push((
            tx,
            tokio::spawn(call(
                router.clone(),
                put(Some(tenant), None).body(body).unwrap(),
            )),
        ));
        let n = held.len();
        until(&api, |c| c.active == n).await;
    }
    assert_eq!(
        api.ingress_counts(),
        IngressCounts {
            active: 3,
            reserved_body_bytes: 3 * BODY,
            tenants: 2
        }
    );
    for (router, tenant) in [(&r2, Some("1")), (&r1, Some("3")), (&r1, None)] {
        let got = call(
            router.clone(),
            put(tenant, None).body(Body::from(write_of(100))).unwrap(),
        )
        .await;
        refused(&got, StatusCode::TOO_MANY_REQUESTS, "overloaded");
        assert_eq!(got.retry_after.as_deref(), Some("1"));
        assert_eq!(got.body["error"]["retryable"], true, "{}", got.body);
    }
    // Tenant 2's one request ends; tenant 1 is still at its own allowance, tenant 3 is not.
    let (tx, task) = held.pop().unwrap();
    tx.send(Ok(bytes::Bytes::from(write_of(100)))).unwrap();
    drop(tx);
    assert_eq!(task.await.unwrap().status, StatusCode::OK);
    let got = call(
        r1.clone(),
        put(Some("1"), None)
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    refused(&got, StatusCode::TOO_MANY_REQUESTS, "overloaded");
    let got = call(
        r1.clone(),
        put(Some("3"), None)
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    for (tx, task) in held {
        drop(tx);
        task.await.unwrap();
    }
    assert_eq!(api.ingress_counts(), IngressCounts::default());
    // The body pool alone refuses: room for two bodies, four slots.
    let (api, _store) = serve(private(), limits(BODY, 4, 4, 2 * BODY + BODY / 2));
    let mut held = Vec::new();
    for tenant in ["1", "2"] {
        let (tx, body) = channel();
        held.push((
            tx,
            tokio::spawn(call(
                api.clone().router(),
                put(Some(tenant), None).body(body).unwrap(),
            )),
        ));
        let n = held.len();
        until(&api, |c| c.active == n).await;
    }
    let got = call(
        api.clone().router(),
        put(Some("3"), None)
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    refused(&got, StatusCode::TOO_MANY_REQUESTS, "overloaded");
    drop(held);
}

#[tokio::test]
async fn ingress_permits_recover_after_all_exit_paths() {
    let (api, _store) = serve(private(), limits(256, 4, 2, 1024));
    let router = api.clone().router();
    // Completion, and a refusal by the handler.
    let ok = call(
        router.clone(),
        put(Some("1"), None)
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
    let bad = call(
        router.clone(),
        put(Some("1"), None).body(Body::from("{not json")).unwrap(),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.ingress_counts(), IngressCounts::default());
    // A dropped connection: the body fails part way.
    let (tx, body) = channel();
    let task = tokio::spawn(call(
        router.clone(),
        put(Some("1"), None).body(body).unwrap(),
    ));
    until(&api, |c| c.active == 1).await;
    tx.send(Ok(bytes::Bytes::from_static(b"{\"documents\":")))
        .unwrap();
    tx.send(Err(std::io::Error::other("reset"))).unwrap();
    let got = task.await.unwrap();
    assert_eq!(got.status, StatusCode::BAD_REQUEST, "{}", got.body);
    assert_eq!(api.ingress_counts(), IngressCounts::default());
    // Cancellation: the request's future is dropped while its body is pending.
    let (_tx, body) = channel();
    let task = tokio::spawn(call(
        router.clone(),
        put(Some("1"), None).body(body).unwrap(),
    ));
    until(&api, |c| c.active == 1).await;
    task.abort();
    let _ = task.await;
    assert_eq!(api.ingress_counts(), IngressCounts::default());
    // Distinct-tenant churn leaves no entry behind; the live table never exceeds the slots.
    for t in 0..200u32 {
        let got = call(
            router.clone(),
            put(Some(&t.to_string()), None)
                .body(Body::from("{"))
                .unwrap(),
        )
        .await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert_eq!(api.ingress_counts(), IngressCounts::default());
    }
    let mut held = Vec::new();
    for t in 0..4 {
        let (tx, body) = channel();
        held.push((
            tx,
            tokio::spawn(call(
                router.clone(),
                put(Some(&t.to_string()), None).body(body).unwrap(),
            )),
        ));
        until(&api, |c| c.active == t + 1).await;
    }
    assert_eq!(api.ingress_counts().tenants, 4);
    drop(held);
    until(&api, |c| c == IngressCounts::default()).await;
    let ok = call(
        router.clone(),
        put(Some("1"), None)
            .body(Body::from(write_of(100)))
            .unwrap(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(api.ingress_counts(), IngressCounts::default());
}

#[tokio::test(start_paused = true)]
async fn a_body_that_misses_its_deadline_is_refused_and_released() {
    let limits = RequestLimits::builder()
        .body_read_timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let (api, store) = serve(private(), limits);
    let (tx, body) = channel();
    tx.send(Ok(bytes::Bytes::from_static(b"{\"documents\":")))
        .unwrap();
    let got = call(
        api.clone().router(),
        put(Some("1"), None).body(body).unwrap(),
    )
    .await;
    refused(&got, StatusCode::REQUEST_TIMEOUT, "request_timeout");
    assert_eq!(api.ingress_counts(), IngressCounts::default());
    assert_eq!(requests(&store), [0; 4]);
    drop(tx);
}

#[tokio::test]
async fn headerless_private_requests_share_one_cohort() {
    let (api, _store) = serve(private(), limits(64, 4, 1, 256));
    let (tx, body) = channel();
    let held = tokio::spawn(call(
        api.clone().router(),
        Request::builder()
            .method("POST")
            .uri("/v1/admin/fold")
            .body(body)
            .unwrap(),
    ));
    until(&api, |c| c.active == 1).await;
    let got = call(
        api.clone().router(),
        Request::builder()
            .method("GET")
            .uri("/v1/admin/duties")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    refused(&got, StatusCode::TOO_MANY_REQUESTS, "overloaded");
    let duties = call(
        api.clone().router(),
        Request::builder()
            .method("GET")
            .uri("/v1/admin/duties")
            .header("x-pstore-tenant", "4")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(duties.status, StatusCode::OK);
    drop(tx);
    held.await.unwrap();
    assert_eq!(api.ingress_counts(), IngressCounts::default());
}

#[test]
fn request_limit_configuration_precedes_store_and_bind() {
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    for (raw, valid) in [
        (None, true),
        (Some(r#"{"version":1,"active_requests":8}"#), true),
        (Some(r#"{"version":2}"#), false),
        (Some(r#"{"version":1,"body_bytes":0}"#), false),
        (
            Some(r#"{"version":1,"active_requests":2,"tenant_active_requests":4}"#),
            false,
        ),
    ] {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_pstore-server"));
        cmd.env_clear()
            .env("PSTORE_LANE", "1")
            .env("PSTORE_BACKEND", "memory")
            .env("PSTORE_ALLOW_UNAUTHENTICATED", "1")
            .env("PSTORE_BIND", reserved.local_addr().unwrap().to_string());
        if let Some(raw) = raw {
            cmd.env("PSTORE_REQUEST_LIMITS_CONFIG", raw);
        }
        if !valid {
            // A store configuration the process would refuse: limits are judged first,
            // before any backend or cache is configured, let alone built.
            cmd.env("PSTORE_CACHE_DIR", "/nonexistent/pstore-m128")
                .env("PSTORE_CACHE_RAM_BYTES", "1048576")
                .env("PSTORE_CACHE_DISK_BYTES", "16777216");
        }
        let got = cmd.output().unwrap();
        assert!(!got.status.success());
        let stderr = String::from_utf8(got.stderr).unwrap();
        if valid {
            assert!(stderr.contains("cannot bind"), "{raw:?}: {stderr}");
        } else {
            assert!(stderr.contains("request limit"), "{raw:?}: {stderr}");
            assert!(!stderr.contains("cannot bind"), "{raw:?}: {stderr}");
            assert!(!stderr.contains("PSTORE_CACHE_DIR"), "{raw:?}: {stderr}");
        }
    }
}

#[test]
fn bare_and_authenticated_constructors_keep_the_default_limits() {
    let store = Accounted::new(MemoryStore::new());
    for api in [
        Api::new(store.clone(), LaneId(1)).unwrap(),
        Api::unsecured(store.clone(), LaneId(1)).unwrap(),
        Api::with_auth(store.clone(), LaneId(1), bearer()).unwrap(),
    ] {
        assert_eq!(api.request_limits(), RequestLimits::default());
    }
}
