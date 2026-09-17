//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What an operator can see — M7f.
//!
//! ⚠️ **No per-tenant label, anywhere.** 1M tenants × four request classes is four million
//! series, which is how a metrics endpoint takes down the thing it observes. The per-tenant
//! numbers already go to the caller that caused them, in every response, and `Meter::usage` is
//! the surface Design rule 13 asks for.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, String) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn write(tenant: &str, id: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant)
        .body(Body::from(format!(
            r#"{{"durability":"durable","documents":[{{"id":"{id}","vector":[1.0,0.5,-0.25,1.0]}}]}}"#
        )))
        .unwrap()
}

fn metrics(tenant: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri("/metrics");
    if let Some(t) = tenant {
        b = b.header("x-pstore-tenant", t);
    }
    b.body(Body::empty()).unwrap()
}

/// The series names and label sets present, ignoring values.
fn series(body: &str) -> BTreeSet<String> {
    body.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect()
}

fn value(body: &str, series: &str) -> u64 {
    body.lines()
        .find(|l| l.starts_with(series))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no series {series} in:\n{body}"))
}

#[tokio::test]
async fn metrics_expose_totals_and_never_a_tenant() {
    let api = api();
    send(&api, write("1234567", "a")).await;
    send(&api, write("7654321", "b")).await;

    let (status, body) = send(&api, metrics(None)).await;
    assert_eq!(status, StatusCode::OK);
    for class in ["read", "write", "list", "delete"] {
        assert!(
            body.contains(&format!("pstore_blob_requests_total{{class=\"{class}\"}}")),
            "no total for {class}:\n{body}"
        );
    }
    assert!(body.contains("pstore_http_requests_total"), "{body}");
    assert!(body.contains("pstore_refusals_total"), "{body}");

    // ⚠️ Neither tenant id appears anywhere -- not as a label, not in a value.
    for tenant in ["1234567", "7654321"] {
        assert!(
            !body.contains(tenant),
            "tenant {tenant} leaked into /metrics:\n{body}"
        );
    }
}

#[tokio::test]
async fn metrics_need_no_tenant_header() {
    let api = api();
    send(&api, write("7", "a")).await;
    // ⚠️ A warmup, and the comparison excludes `/metrics`'s own row: the route counter is
    // incremented by the layer AFTER the handler formats the body, so the first response
    // cannot contain its own series and the second can. Spec review found the earlier
    // "identical series set" false for exactly that pair.
    send(&api, metrics(None)).await;
    let (_, anonymous) = send(&api, metrics(None)).await;
    let (status, with_tenant) = send(&api, metrics(Some("7"))).await;
    assert_eq!(status, StatusCode::OK);

    let strip = |b: &str| -> BTreeSet<String> {
        series(b)
            .into_iter()
            .filter(|s| !s.contains("/metrics"))
            .collect()
    };
    assert_eq!(
        strip(&anonymous),
        strip(&with_tenant),
        "supplying a tenant changed what /metrics reports"
    );
}

#[tokio::test]
async fn the_counters_move_with_the_work() {
    let api = api();
    // ⚠️ **Two tenants**, because for one the equality is guaranteed by the plumbing: the
    // response's cost and the total are diffs of the same `bill()` call. Summing two callers'
    // costs is the assertion the plumbing does not make for us.
    let mut expected = 0u64;
    for (tenant, id) in [("11", "a"), ("22", "b")] {
        let (status, body) = send(&api, write(tenant, id)).await;
        assert_eq!(status, StatusCode::OK);
        let cost: serde_json::Value = serde_json::from_str(&body).unwrap();
        expected += cost["cost"]["blob_writes"].as_u64().unwrap();
    }
    let (_, body) = send(&api, metrics(None)).await;
    assert_eq!(
        value(&body, "pstore_blob_requests_total{class=\"write\"}"),
        expected,
        "the write total is not the sum of what the callers were told:\n{body}"
    );
    // ⚠️ **The route counter's VALUE, not its presence.** A mutation sweep walked through the
    // earlier test: replacing the increment with `*= 1` leaves every series in place at zero,
    // and a test that only greps for the name cannot tell a counter from a label.
    assert_eq!(
        value(
            &body,
            "pstore_http_requests_total{route=\"/v1/indexes/{index}/documents\",status=\"200\"}"
        ),
        2,
        "two writes were not counted as two:\n{body}"
    );
}

#[tokio::test]
async fn a_refusal_is_counted_under_its_own_code() {
    let api = api();
    // Two different refusals, and a success that must not be counted as either.
    send(&api, write("7", "a")).await;
    let no_tenant = Request::builder()
        .method("GET")
        .uri("/v1/indexes")
        .body(Body::empty())
        .unwrap();
    send(&api, no_tenant).await;
    let bad_query = Request::builder()
        .method("POST")
        .uri("/v1/indexes/docs/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":0}"#))
        .unwrap();
    send(&api, bad_query).await;

    let (_, body) = send(&api, metrics(None)).await;
    assert_eq!(
        value(&body, "pstore_refusals_total{code=\"tenant_required\"}"),
        1,
        "{body}"
    );
    assert_eq!(
        value(&body, "pstore_refusals_total{code=\"bad_request\"}"),
        1,
        "{body}"
    );
    // ⚠️ And an unmatched route is counted as one, without inventing a code for it.
    let nowhere = Request::builder()
        .method("GET")
        .uri("/nope")
        .body(Body::empty())
        .unwrap();
    send(&api, nowhere).await;
    let (_, body) = send(&api, metrics(None)).await;
    assert!(
        body.contains("pstore_http_requests_total{route=\"<unmatched>\""),
        "an unrouted request was not counted:\n{body}"
    );
}
