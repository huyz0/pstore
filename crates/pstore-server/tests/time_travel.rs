//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `as_of` through the API — M7e.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use std::sync::Arc;
use tower::ServiceExt;

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn write(id: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(format!(
            r#"{{"durability":"durable","documents":[{{"id":"{id}","vector":[1.0,0.5,-0.25,1.0]}}]}}"#
        )))
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

fn fold() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/admin/fold")
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_api_answers_as_of_and_refuses_past_the_horizon() {
    let api = api();
    send(&api, write("first")).await;
    let (_, body) = send(&api, fold()).await;
    let then = body["epoch"].as_u64().unwrap();
    send(&api, write("second")).await;
    send(&api, fold()).await;

    // The present sees both.
    let (status, body) = send(&api, query(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":5}"#)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"].as_array().unwrap().len(), 2, "{body}");

    // ⚠️ The past sees what the past saw, and `meta.epoch` says which past -- without that, a
    // stale answer is indistinguishable from a fresh one, which is worse than no answer.
    let (status, body) = send(
        &api,
        query(&format!(
            r#"{{"vector":[1.0,0.5,-0.25,1.0],"top_k":5,"as_of":{then}}}"#
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["results"][0]["id"], "first");
    assert_eq!(body["meta"]["epoch"], then);

    // An epoch that has not happened is refused rather than answered with today.
    let (status, body) = send(
        &api,
        query(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":5,"as_of":9999}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "time_travel_horizon");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("9999"),
        "the refusal must name the epoch asked for: {body}"
    );
    assert_eq!(
        body["error"]["retryable"], false,
        "asking for the same impossible epoch again will fail the same way"
    );

    // ⚠️ And an index that did not exist at that epoch is a `404`, by the same predicate a
    // live query uses -- not an empty `200`, which would say "it is there and empty".
    let req = Request::builder()
        .method("POST")
        .uri("/v1/indexes/absent/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(format!(
            r#"{{"vector":[1.0,0.5,-0.25,1.0],"top_k":5,"as_of":{then}}}"#
        )))
        .unwrap();
    let (status, body) = send(&api, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "index_not_found");

    // ⚠️ **The horizon arm, end to end.** Code review pointed out that the future refusal and
    // the reaped one share a mapping but not a test: the fixture had no `gc` in it, so the
    // half that actually costs a caller their history was unverified through the API.
    for _ in 0..3 {
        send(&api, write("more")).await;
        send(&api, fold()).await;
    }
    let (status, body) = send(&api, gc()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let horizon = body["reaped_before"].as_u64().unwrap();
    assert!(horizon > 0, "the fixture reaped nothing: {body}");
    // ⚠️ The epoch a reap reports is the one it committed, not the last one a FOLD committed.
    // Code review measured the difference: a gc that committed 6 answered 5, and every query
    // from that engine then reported 5 until the next fold.
    let reaped_at = body["epoch"].as_u64().unwrap();
    let (_, body) = send(&api, query(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":5}"#)).await;
    assert_eq!(
        body["meta"]["epoch"], reaped_at,
        "a query after a reap reported a different epoch than the reap did: {body}"
    );

    let (status, body) = send(
        &api,
        query(&format!(
            r#"{{"vector":[1.0,0.5,-0.25,1.0],"top_k":5,"as_of":{}}}"#,
            horizon - 1
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "time_travel_horizon");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&(horizon - 1).to_string()) && message.contains(&horizon.to_string()),
        "the refusal must name both numbers, so a caller learns the bound: {message}"
    );
}

fn gc() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/admin/gc?retention=1")
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap()
}
