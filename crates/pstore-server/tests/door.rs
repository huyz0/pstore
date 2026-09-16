//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M7c.1 — the door: who the caller is, and what it is told when it is wrong.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use std::sync::Arc;
use tower::ServiceExt;

/// A server over an in-memory store that can fence.
fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn write_req(tenant: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json");
    if let Some(t) = tenant {
        b = b.header("x-pstore-tenant", t);
    }
    b.body(Body::from(body.to_owned())).unwrap()
}

const ONE_DOC: &str = r#"{"documents":[{"id":"a","vector":[1.0,0.0,0.0,0.0]}]}"#;

#[tokio::test]
async fn a_missing_tenant_header_is_refused() {
    // ⚠️ **Never defaulted.** A default tenant is a cross-tenant data leak with a plausible
    // name, and it passes every single-tenant test ever written against this server.
    for header in [None, Some(""), Some("not-a-number"), Some("-1")] {
        let (status, body) = send(&api(), write_req(header, ONE_DOC)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "tenant header {header:?} was accepted"
        );
        assert_eq!(body["error"]["code"], "tenant_required");
        assert_eq!(body["error"]["retryable"], false);
    }
}

#[tokio::test]
async fn the_error_table_maps_every_row() {
    let api = api();

    // Malformed JSON is the client's fault, and it is not retryable.
    let (status, body) = send(&api, write_req(Some("7"), "{not json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");

    // A durability level we do not implement is REFUSED, never accepted and downgraded.
    let (status, body) = send(
        &api,
        write_req(
            Some("7"),
            r#"{"durability":"async","documents":[{"id":"a","vector":[1.0]}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "unsupported_durability");

    // An index nobody has written is a 404, decided by asking rather than by an empty result.
    let (status, body) = send(
        &api,
        Request::builder()
            .method("POST")
            .uri("/v1/indexes/absent/query")
            .header("content-type", "application/json")
            .header("x-pstore-tenant", "7")
            .body(Body::from(r#"{"vector":[1.0,0.0,0.0,0.0],"top_k":5}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "index_not_found");

    // A query with no legs, and a top_k of zero, are requests we cannot answer.
    send(&api, write_req(Some("7"), ONE_DOC)).await;
    for (body_json, why) in [
        (r#"{"top_k":5}"#, "no legs"),
        (r#"{"vector":[1.0,0.0,0.0,0.0],"top_k":0}"#, "top_k zero"),
    ] {
        let (status, body) = send(
            &api,
            Request::builder()
                .method("POST")
                .uri("/v1/indexes/docs/query")
                .header("content-type", "application/json")
                .header("x-pstore-tenant", "7")
                .body(Body::from(body_json))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why} was accepted");
        assert_eq!(body["error"]["code"], "bad_request", "{why}");
    }
}

#[tokio::test]
async fn a_backend_that_cannot_fence_is_refused_before_the_socket_is_bound() {
    // ⚠️ M7a made `admits_durable_writes()` public for exactly this caller. Without the door
    // guard a *batched* write to a divergent backend answers 200 -- `Engine::write` calls
    // only `check_storable`, and the fencing guard is at the flush.
    let divergent = pstore_testkit::claims::Claims::divergent_cas("wildcard accepted and ignored");
    let refused = Api::new(Accounted::new(divergent), LaneId(1));
    assert!(
        refused.is_err(),
        "a backend that cannot fence was allowed to serve"
    );
}

#[tokio::test]
async fn a_just_written_index_is_not_a_404() {
    // ⚠️ The contradiction spec review round 2 found: existence decided on HEAD alone would
    // 404 the index that criterion 2 requires this same server to answer from its memtable.
    let api = api();
    let (status, _) = send(&api, write_req(Some("7"), ONE_DOC)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        &api,
        Request::builder()
            .method("POST")
            .uri("/v1/indexes/docs/query")
            .header("content-type", "application/json")
            .header("x-pstore-tenant", "7")
            .body(Body::from(r#"{"vector":[1.0,0.0,0.0,0.0],"top_k":5}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["id"], "a");
}
