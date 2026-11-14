//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A query reserves its whole plan's candidate rows before it runs (M212): every leg, window,
//! sub-query and facet, against a tenant's pool and the server's.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_server::{Api, RequestLimits};
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt;

/// A store that, once armed, parks its next read until released: a query held mid-run.
#[derive(Clone, Default)]
struct Parked {
    inner: MemoryStore,
    armed: Arc<AtomicBool>,
    arrived: Arc<Notify>,
    release: Arc<Notify>,
}

impl Parked {
    async fn park(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.arrived.notify_one();
            self.release.notified().await;
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Parked {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.park().await;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.park().await;
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.park().await;
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.park().await;
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.park().await;
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        self.park().await;
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        self.park().await;
        self.inner.get_suffix_as(key, n, class).await
    }
    async fn get_immutable(
        &self,
        key: &Key,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        self.park().await;
        self.inner.get_immutable(key, class).await
    }
}

type A = Arc<Api<Parked>>;

/// The tenant pool every boundary is measured against.
const R: u64 = 60;
const T: u128 = 51;
const U: u128 = 52;

struct Got {
    status: StatusCode,
    body: Value,
    retry_after: Option<String>,
}

async fn send(api: &A, tenant: u128, method: &str, uri: &str, body: &Value) -> Got {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let retry_after = res
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Got {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        retry_after,
    }
}

fn limits(all: u64, each: u64) -> RequestLimits {
    RequestLimits::builder()
        .query_rows(all)
        .tenant_query_rows(each)
        .build()
        .unwrap()
}

/// 40 rows for each tenant, folded.
async fn serve(all: u64, each: u64) -> (A, Accounted<Parked>, Parked) {
    let parked = Parked::default();
    let store = Accounted::new(parked.clone());
    let api = Api::with_limits(
        store.clone(),
        LaneId(1),
        None,
        pstore_server::Auth::from_vars(|k| {
            (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())
        })
        .unwrap(),
        limits(all, each),
    )
    .unwrap();
    let docs: Vec<Value> = (0..40)
        .map(|i| {
            json!({"id": format!("d{i:02}"), "vector": [1.0, f64::from(i) / 40.0],
                   "text": format!("common word{}", i % 5),
                   "attributes": {"n": i, "tag": format!("t{}", i % 4)}})
        })
        .collect();
    for tenant in [T, U] {
        let w = json!({"durability": "durable", "documents": docs});
        let got = send(&api, tenant, "PUT", "/v1/indexes/docs/documents", &w).await;
        assert_eq!(got.status, StatusCode::OK, "{}", got.body);
        let got = send(&api, tenant, "POST", "/v1/admin/fold", &json!({})).await;
        assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    }
    (api, store, parked)
}

fn spent(store: &Accounted<Parked>, tenant: u128) -> [u64; 3] {
    [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(tenant), c))
}

async fn query(api: &A, tenant: u128, q: &Value) -> Got {
    send(api, tenant, "POST", "/v1/indexes/docs/query", q).await
}

/// `fits` succeeds, and `over` is 413 naming `tenant_query_rows`, with no blob request.
async fn boundary(api: &A, store: &Accounted<Parked>, fits: &Value, over: &Value) {
    let got = query(api, T, fits).await;
    assert_eq!(got.status, StatusCode::OK, "{fits} -> {}", got.body);
    let before = spent(store, T);
    let got = query(api, T, over).await;
    assert_eq!(
        got.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{over} -> {}",
        got.body
    );
    assert_eq!(
        got.body["error"]["code"], "request_too_large",
        "{}",
        got.body
    );
    let msg = got.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("tenant_query_rows"), "{msg}");
    assert_eq!(
        before,
        spent(store, T),
        "{over}: the refusal made blob requests"
    );
    assert_eq!(api.reserved_query_rows(), 0);
}

const V: [f64; 2] = [1.0, 0.5];

fn hybrid(k: u64) -> Value {
    json!({"vector": V, "text": "word2", "top_k": k})
}

fn order(k: u64) -> Value {
    json!({"rank_by": ["n", "asc"], "top_k": k})
}

#[tokio::test]
async fn every_leg_and_sub_query_counts_up_to_the_tenants_pool() {
    let (api, store, _) = serve(1 << 20, R).await;
    // Two legs at the window, and the window: 3k.
    boundary(&api, &store, &hybrid(20), &hybrid(21)).await;
    // Three hybrid sub-queries (45) and an order of 15: 60; an order of 16 is one row more.
    let multi = |last: u64| json!({"queries": [hybrid(5), hybrid(5), hybrid(5), order(last)]});
    boundary(&api, &store, &multi(15), &multi(16)).await;
}

#[tokio::test]
async fn windows_offsets_aggregations_and_facets_each_count() {
    let (api, store, _) = serve(1 << 20, R).await;
    let boosted = |w: u64| {
        json!({"vector": V, "top_k": 1,
               "boost": {"window": w, "terms": [{"attr": "n", "fn": "linear", "weight": 0.001}]}})
    };
    boundary(&api, &store, &boosted(30), &boosted(31)).await;
    let offset = |o: u64| json!({"vector": V, "top_k": 20, "offset": o});
    boundary(&api, &store, &offset(10), &offset(11)).await;
    let ordered = |o: u64| json!({"rank_by": ["n", "asc"], "top_k": 50, "offset": o});
    boundary(&api, &store, &ordered(10), &ordered(11)).await;
    let counted =
        |k: u64| json!({"aggregate_by": {"c": ["Count"]}, "group_by": ["tag"], "top_k": k});
    boundary(&api, &store, &counted(60), &counted(61)).await;
    let faceted = |g: u64| {
        json!({"vector": V, "top_k": 20,
               "facets": {"aggregate_by": {"c": ["Count"]}, "group_by": ["tag"], "top_k": g}})
    };
    boundary(&api, &store, &faceted(20), &faceted(21)).await;
}

#[tokio::test]
async fn a_short_pool_is_overloaded_and_another_tenant_is_not() {
    let (api, store, _) = serve(1 << 20, R).await;
    let held = api.hold_query_rows_for_test(TenantId(T), 30).unwrap();
    let fits = order(30);
    let over = order(31);
    assert_eq!(query(&api, T, &fits).await.status, StatusCode::OK);
    let before = spent(&store, T);
    let got = query(&api, T, &over).await;
    assert_eq!(got.status, StatusCode::TOO_MANY_REQUESTS, "{}", got.body);
    assert_eq!(got.body["error"]["code"], "overloaded", "{}", got.body);
    assert_eq!(got.retry_after.as_deref(), Some("1"));
    assert_eq!(before, spent(&store, T), "the refusal made blob requests");
    // Another tenant's pool is its own.
    assert_eq!(query(&api, U, &order(60)).await.status, StatusCode::OK);
    assert_eq!(api.reserved_query_rows(), 30);
    drop(held);
    assert_eq!(query(&api, T, &over).await.status, StatusCode::OK);
    // The server's pool binds every tenant: all of it held by T refuses U's single row.
    let (api, store, _) = serve(R, R).await;
    let held = api.hold_query_rows_for_test(TenantId(T), R).unwrap();
    let before = spent(&store, U);
    let got = query(&api, U, &order(1)).await;
    assert_eq!(got.status, StatusCode::TOO_MANY_REQUESTS, "{}", got.body);
    assert_eq!(before, spent(&store, U), "the refusal made blob requests");
    drop(held);
    assert_eq!(query(&api, U, &order(1)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_reservation_is_held_while_running_and_returned_on_every_path() {
    let (api, _, parked) = serve(1 << 20, R).await;
    // Held mid-run: the first read parks, and the reservation is the plan's.
    parked.armed.store(true, Ordering::SeqCst);
    let running = tokio::spawn({
        let api = Arc::clone(&api);
        async move { query(&api, T, &hybrid(7)).await }
    });
    parked.arrived.notified().await;
    assert_eq!(api.reserved_query_rows(), 21);
    parked.release.notify_one();
    assert_eq!(running.await.unwrap().status, StatusCode::OK);
    assert_eq!(api.reserved_query_rows(), 0);
    // A multi-query too: its sub-queries' sum, held until it is answered.
    parked.armed.store(true, Ordering::SeqCst);
    let running = tokio::spawn({
        let api = Arc::clone(&api);
        async move { query(&api, T, &json!({"queries": [hybrid(3), order(4)]})).await }
    });
    parked.arrived.notified().await;
    assert_eq!(api.reserved_query_rows(), 13);
    parked.release.notify_one();
    assert_eq!(running.await.unwrap().status, StatusCode::OK);
    assert_eq!(api.reserved_query_rows(), 0);
    // A 404 found while running.
    let got = send(&api, T, "POST", "/v1/indexes/nothing/query", &order(5)).await;
    assert_eq!(got.status, StatusCode::NOT_FOUND, "{}", got.body);
    assert_eq!(api.reserved_query_rows(), 0);
    // A 413, and a 429.
    assert_eq!(
        query(&api, T, &order(61)).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let held = api.hold_query_rows_for_test(TenantId(T), R).unwrap();
    assert_eq!(
        query(&api, T, &order(1)).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(api.reserved_query_rows(), R);
    drop(held);
    assert_eq!(api.reserved_query_rows(), 0);
}

#[tokio::test]
async fn an_external_query_reserves_its_window() {
    // `docs` has no external source, so a query that fits is answered by the engine, whatever
    // it says; one whose window is past the pool is refused before anything is read.
    let (api, store, _) = serve(4 * R, R).await;
    let uri = "/v1/indexes/docs/external/query";
    let ask = |window: u64| json!({"vector": [1.0, 0.5], "k": 10, "window": window});
    let got = send(&api, T, "POST", uri, &ask(R)).await;
    assert_ne!(got.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", got.body);
    let before = spent(&store, T);
    let got = send(&api, T, "POST", uri, &ask(R + 1)).await;
    assert_eq!(got.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", got.body);
    assert!(
        got.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tenant_query_rows"),
        "{}",
        got.body
    );
    assert_eq!(spent(&store, T), before, "a refusal costs no request");
}
