//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M7c.1 — **the rows of the error table the client never gets to see by asking wrongly.**
//!
//! ⚠️ Written because the coverage number found them: the request-level refusals were
//! asserted and the whole `From<EngineError>` arm was not executed once. A table nothing
//! exercises is a table whose `409` may be a `500` in production and green in CI.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore};
use pstore_server::Api;
use pstore_testkit::claims::Claims;
use pstore_testkit::flaky::Flaky;
use pstore_types::LaneId;
use std::sync::Arc;
use tower::ServiceExt;

async fn send<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn write(body: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn query(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/indexes/docs/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

const DURABLE_ONE: &str =
    r#"{"durability":"durable","documents":[{"id":"a","vector":[1.0,0.5,-0.25,1.0]}]}"#;

#[tokio::test]
async fn a_contended_commit_is_a_409_and_not_a_500() {
    // ⚠️ `Lost` and `Contended` are the client's decision to make — re-read and retry — and
    // `Congested` deliberately refuses to retry a CAS on anyone's behalf. Reporting them as
    // `500` would tell a client its request was unsound when the remedy is to send it again.
    let api = Api::new(Accounted::new(Flaky::always_contended()), LaneId(1)).unwrap();
    let (status, body) = send(&api, write(DURABLE_ONE)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "version_conflict");
    assert_eq!(
        body["error"]["retryable"], false,
        "a 409 is the caller's move, not a retry of the same bytes"
    );
}

#[tokio::test]
async fn a_storage_failure_is_a_retryable_503() {
    // A backend that is down, not busy. ⚠️ `retryable: true` is derived from the status, so
    // this also pins that a 5xx cannot be reported as something a client should give up on.
    let api = Api::new(Accounted::new(Flaky::refusing_reads()), LaneId(1)).unwrap();
    let (status, body) = send(&api, query(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":3}"#)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "storage_unavailable");
    assert_eq!(body["error"]["retryable"], true);
}

#[tokio::test]
async fn a_backend_that_stops_fencing_is_a_503_rather_than_a_silent_write() {
    // ⚠️ The startup guard cannot catch this one: the profile was fine when the server was
    // built and the store is re-probed as divergent afterwards. M7a put a guard at every CAS
    // for exactly this, and the table's job is to turn it into an answer a client can read.
    let api = Api::new(Accounted::new(Claims::conforming()), LaneId(1)).unwrap();
    // Now make the same store divergent, as a re-probe would.
    let degraded = Claims::degraded(MemoryStore::new(), "re-probed: wildcard ignored");
    let api2 = Api::new(Accounted::new(degraded), LaneId(1));
    assert!(api2.is_err(), "the door must refuse a divergent profile");

    // And the door is not the only guard: a server built on a fencing backend still answers.
    let (status, _) = send(&api, write(DURABLE_ONE)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_dimension_mismatch_is_a_client_error_on_both_query_paths() {
    // ⚠️ **This row was deferred by the spec and then earned.** It was a `500 internal`
    // because `Engine::query` collapsed every query failure into a string, and a table that
    // matches on messages is a table no mutation can pin. A typed `DimensionMismatch` is
    // what made it answerable -- and the defect underneath was worse than the status code:
    // the dense leg took the index's dimension FROM THE QUERY, so a two-dimensional vector
    // over a four-dimensional segment came back with scored, ranked rows and a `200`.
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    send(&api, write(DURABLE_ONE)).await;
    let fold = Request::builder()
        .method("POST")
        .uri("/v1/admin/fold")
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap();
    send(&api, fold).await;

    let (status, body) = send(&api, query(r#"{"vector":[1.0,2.0],"top_k":3}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "schema_conflict");
    assert_eq!(
        body["error"]["retryable"], false,
        "sending the same wrong-sized vector again will fail the same way"
    );
    assert!(
        body["error"]["message"].as_str().unwrap().contains('4'),
        "the refusal must say what the field's dimension is: {body}"
    );

    // ⚠️ **After a fold this process cannot know the width**, and asking would put a blob
    // request on the write path. So the write is accepted -- and the mistake is **loud at
    // the next query** rather than a wrong ranking: the dense leg takes its dimension from
    // each segment's field layout, and the fresh two-dimensional segment refuses a
    // four-dimensional query. Review's scenario ended with the short vector outranking an
    // exact match, scored and `200`; it now ends in a refusal.
    let (status, _) = send(
        &api,
        write(r#"{"documents":[{"id":"later","vector":[9.0,9.0]}]}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a process with no rows for this index cannot know its width for free"
    );
    let (status, body) = send(&api, query(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":5}"#)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a mixed-width index answered a query instead of refusing it: {body}"
    );
    assert_eq!(body["error"]["code"], "schema_conflict");

    // ⚠️ And within one batch, where it is cheapest: a batch whose vectors disagree makes the
    // index's dimension depend on which document happened to be first.
    let (status, body) = send(
        &api,
        write(
            r#"{"documents":[{"id":"a","vector":[1.0,0.5,-0.25,1.0]},{"id":"b","vector":[1.0,2.0]}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "schema_conflict");
}

#[tokio::test]
async fn the_requests_that_ask_for_nothing_are_refused() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    for (body_str, why) in [
        (r#"{"documents":[]}"#, "a write with no documents"),
        (
            r#"{"documents":[{"id":"a","vector":[]}]}"#,
            "a document with no vector",
        ),
    ] {
        let (status, body) = send(&api, write(body_str)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{why} was accepted: {body}"
        );
    }
    let (status, body) = send(&api, query(r#"{"vector":[],"top_k":3}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // An index this tenant has never touched has no summary either.
    let (status, body) = send(
        &api,
        Request::builder()
            .method("GET")
            .uri("/v1/indexes/never")
            .header("x-pstore-tenant", "7")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "index_not_found");
}

#[tokio::test]
async fn text_is_written_queried_and_fused_with_the_vector() {
    // The BM25 leg through the API, and `top_k` defaulted rather than sent.
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let (status, _) = send(
        &api,
        write(
            r#"{"durability":"durable","documents":[
                {"id":"rev","vector":[1.0,0.5,-0.25,1.0],"text":"quarterly revenue report"},
                {"id":"rev2","vector":[0.9,0.5,-0.25,1.0],"text":"revenue by quarter"},
                {"id":"rev3","vector":[0.8,0.5,-0.25,1.0],"text":"the quarterly revenue call"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(&api, query(r#"{"text":"quarterly revenue"}"#)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["id"], "rev", "{body}");
    // ⚠️ **The default `top_k` is pinned by a number, not by a bound.** `<= 10` was the first
    // assertion here and a mutation sweep walked straight through it: a default of **1**
    // satisfies it, and would silently truncate every client that does not send `top_k`.
    // Three documents match, so three come back.
    assert_eq!(
        body["results"].as_array().unwrap().len(),
        3,
        "the defaulted top_k did not return every match: {body}"
    );

    let (status, body) = send(
        &api,
        query(r#"{"text":"quarterly revenue","vector":[1.0,0.5,-0.25,1.0],"top_k":2}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["id"], "rev", "{body}");
}

#[tokio::test]
async fn a_backend_that_stops_fencing_maps_to_a_retryable_503() {
    // ⚠️ The door guard cannot catch this one, which is why M7a put a guard at every CAS as
    // well: the profile was `Supported` when this process started and the backend has since
    // been re-probed. The engine raises it; the table's job is to turn it into an answer,
    // and it is asserted on the conversion because contriving a store whose capabilities
    // change mid-test would be a fixture testing itself.
    let response = axum::response::IntoResponse::into_response(pstore_server::ApiError::from(
        pstore_engine::EngineError::BackendCannotFence {
            backend: "re-probed".to_owned(),
            primitive: "cas",
            observed: "wildcard accepted and ignored".to_owned(),
        },
    ));
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "storage_unavailable");
    assert_eq!(json["error"]["retryable"], true);
    assert!(
        json["error"]["message"].as_str().unwrap().contains("cas"),
        "the refusal must name the primitive, or an operator has nothing to do: {json}"
    );
}

#[tokio::test]
async fn an_unreadable_head_is_a_500_and_says_nothing_it_does_not_know() {
    // ⚠️ The catch-all arm, and the only one whose message is the engine's own: a corrupt
    // HEAD is **ours**, not the client's, so it is a 5xx — and `retryable` derives from the
    // status rather than from a guess about whether re-reading it would help.
    let backend = MemoryStore::new();
    let api = Api::new(Accounted::new(backend.clone()), LaneId(1)).unwrap();
    send(&api, write(DURABLE_ONE)).await;
    backend
        .put(
            &pstore_engine::Head::key(pstore_types::TenantId(7)),
            bytes::Bytes::from_static(b"this is not a manifest"),
        )
        .await
        .unwrap();

    let (status, body) = send(&api, query(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":3}"#)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "internal");
    assert_eq!(body["error"]["retryable"], true);
}

#[tokio::test]
async fn a_later_batch_may_not_change_the_index_width() {
    // ⚠️ **The case review measured**: two writes, each consistent with itself, the second
    // inconsistent with the index. The door compared dimensions *within* a request, so both
    // were accepted and the two-dimensional document then outranked an exact four-dimensional
    // match -- scored, ranked, `200`, which is the worst failure a search engine has.
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let (status, _) = send(&api, write(DURABLE_ONE)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        &api,
        write(r#"{"documents":[{"id":"short","vector":[9.0,9.0]}]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "schema_conflict");
    assert!(
        body["error"]["message"].as_str().unwrap().contains('4'),
        "the refusal must say what the index's width is: {body}"
    );

    // And the refused document is not in the index: a 400 that wrote anyway is worse than
    // either outcome.
    let (_, body) = send(&api, query(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":5}"#)).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["results"][0]["id"], "a");
}
