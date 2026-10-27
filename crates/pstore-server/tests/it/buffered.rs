//! M141: a tenant's batched rows are flushed at one bound, and its buffered rows refused at
//! another.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_engine::{BufferBounds, Buffered};
use pstore_format::Document;
use pstore_server::{Api, Auth, FoldPolicy, RequestLimits};
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn private() -> Auth {
    Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).unwrap()
}

fn limits(flush: u64, cap: u64) -> RequestLimits {
    RequestLimits::builder()
        .tenant_flush_bytes(flush)
        .tenant_buffered_bytes(cap)
        .build()
        .unwrap()
}

fn serve<S: BlobStore + Clone + 'static>(
    store: S,
    limits: RequestLimits,
) -> (Arc<Api<S>>, Accounted<S>) {
    let store = Accounted::new(store);
    let api = Api::with_limits(store.clone(), LaneId(1), None, private(), limits).unwrap();
    (api, store)
}

/// The footprint every one-row write here buffers: a three-character id, two components.
fn f() -> u64 {
    Document::new("d00", vec![1.0, 0.5]).footprint()
}

fn totals<S: BlobStore>(store: &Accounted<S>) -> [u64; 4] {
    [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ]
    .map(|c| store.total(c))
}

async fn send<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: u128,
    method: &str,
    path: &str,
    body: &Value,
) -> (StatusCode, Value, Option<String>) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = api.clone().router().oneshot(req).await.unwrap();
    let status = res.status();
    let retry = res
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        retry,
    )
}

async fn write<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: u128,
    id: &str,
    durability: &str,
) -> (StatusCode, Value, Option<String>) {
    let body = json!({"durability": durability,
        "documents": [{"id": id, "vector": [1.0, 0.5]}]});
    send(api, tenant, "PUT", "/v1/indexes/docs/documents", &body).await
}

async fn buffered<S: BlobStore + 'static>(api: &Arc<Api<S>>, tenant: u128) -> Buffered {
    api.hold_engine_for_test(TenantId(tenant)).await.buffered()
}

async fn ids<S: BlobStore + 'static>(api: &Arc<Api<S>>, tenant: u128) -> Vec<String> {
    let (s, b, _) = send(
        api,
        tenant,
        "POST",
        "/v1/indexes/docs/query",
        &json!({"vector": [1.0, 0.5], "top_k": 100}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut ids: Vec<String> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn batched_rows_flush_at_the_tenant_flush_bound() {
    let (api, store) = serve(MemoryStore::new(), limits(3 * f(), 64 << 20));
    // Warm the lane: its registration is a first durable write's cost, not this bound's.
    assert_eq!(write(&api, 1, "d00", "durable").await.0, StatusCode::OK);
    let warm = totals(&store);
    for id in ["d01", "d02", "d03"] {
        assert_eq!(write(&api, 1, id, "batched").await.0, StatusCode::OK);
    }
    assert_eq!(
        buffered(&api, 1).await.pending,
        3 * f(),
        "exactly at the bound"
    );
    assert_eq!(
        totals(&store),
        warm,
        "batched rows within the bound cost nothing"
    );
    let before = buffered(&api, 1).await.unfolded;
    let (s, b, _) = write(&api, 1, "d04", "batched").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["durable"], false, "the write itself stays batched");
    let [r, w, l, d] = totals(&store);
    assert_eq!(
        [w, l, d],
        [warm[1] + 1, warm[2], warm[3]],
        "exactly one PUT"
    );
    assert!(r >= warm[0]);
    let now = buffered(&api, 1).await;
    assert_eq!(now.pending, f(), "only the write's own row is pending");
    assert!(now.unfolded > before, "{now:?}");
    assert_eq!(ids(&api, 1).await, ["d00", "d01", "d02", "d03", "d04"]);
}

#[tokio::test]
async fn buffered_rows_refuse_at_the_tenant_bound() {
    // A probe measures what one durable row's bundle holds unfolded.
    let (probe, _) = serve(MemoryStore::new(), RequestLimits::default());
    assert_eq!(write(&probe, 1, "d01", "durable").await.0, StatusCode::OK);
    let u = buffered(&probe, 1).await.unfolded;
    assert!(u >= f(), "a bundle holds at least its row: {u}");
    let cap = u + f();
    let (api, store) = serve(MemoryStore::new(), limits(cap, cap));
    assert_eq!(write(&api, 1, "d01", "durable").await.0, StatusCode::OK);
    assert_eq!(buffered(&api, 1).await.unfolded, u);
    // Exactly at the bound.
    assert_eq!(write(&api, 1, "d02", "batched").await.0, StatusCode::OK);
    let at = totals(&store);
    let (s, b, retry) = write(&api, 1, "d03", "durable").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{b}");
    assert_eq!(b["error"]["code"], "overloaded");
    assert_eq!(retry.as_deref(), Some("1"));
    assert_eq!(totals(&store), at, "a refusal costs no request");
    assert_eq!(
        buffered(&api, 1).await,
        Buffered {
            pending: f(),
            reserved: 0,
            unfolded: u
        },
        "nothing of the refused write is buffered"
    );
    // The refusal asked for a fold: a tick that finds nothing else due takes it.
    let lazy = FoldPolicy {
        age: std::time::Duration::from_secs(86_400),
        bytes: u64::MAX,
        ..FoldPolicy::default()
    };
    assert_eq!(api.fold_due(&lazy).await.folded, 1);
    assert_eq!(api.fold_due(&lazy).await.folded, 0, "one request, one fold");
    assert_eq!(buffered(&api, 1).await.unfolded, 0);
    let (s, b, _) = write(&api, 1, "d03", "durable").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&api, 1).await, ["d01", "d02", "d03"]);
}

#[tokio::test]
async fn one_tenant_at_its_bound_leaves_another_alone() {
    let (api, _store) = serve(MemoryStore::new(), limits(2 * f(), 2 * f()));
    for id in ["d01", "d02"] {
        assert_eq!(write(&api, 1, id, "batched").await.0, StatusCode::OK);
    }
    // Tenant 1 is at its bound; a concurrent request's reservation is what it must not pass.
    let engine = api.hold_engine_for_test(TenantId(2)).await;
    let held = engine
        .reserve_buffer(
            f(),
            BufferBounds {
                flush_at: None,
                cap: 2 * f(),
            },
        )
        .unwrap();
    assert_eq!(write(&api, 2, "d01", "batched").await.0, StatusCode::OK);
    let (s, b, _) = write(&api, 2, "d02", "durable").await;
    assert_eq!(
        s,
        StatusCode::TOO_MANY_REQUESTS,
        "a held reservation counts: {b}"
    );
    drop(held);
    assert_eq!(engine.buffered().reserved, 0);
    assert_eq!(write(&api, 2, "d02", "batched").await.0, StatusCode::OK);
    assert_eq!(buffered(&api, 1).await.pending, 2 * f());
    assert_eq!(buffered(&api, 2).await.pending, 2 * f());
}

#[tokio::test]
async fn a_write_larger_than_the_bound_is_too_large_not_overloaded() {
    let (api, store) = serve(MemoryStore::new(), limits(f() - 1, f() - 1));
    let (s, b, retry) = write(&api, 1, "d01", "batched").await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
    assert_eq!(b["error"]["code"], "request_too_large");
    assert_eq!(retry, None);
    assert_eq!(totals(&store), [0; 4]);
}

#[tokio::test]
async fn a_failed_preflush_changes_nothing() {
    let flaky = pstore_testkit::flaky::Flaky::refusing(&(0..64).collect::<Vec<_>>());
    let (api, _store) = serve(Arc::new(flaky), limits(f(), 64 << 20));
    assert_eq!(write(&api, 1, "d01", "batched").await.0, StatusCode::OK);
    let before = buffered(&api, 1).await;
    let (s, b, _) = write(&api, 1, "d02", "batched").await;
    assert!(s.is_server_error(), "{s}: {b}");
    assert_eq!(buffered(&api, 1).await, before);
    assert_eq!(
        api.hold_engine_for_test(TenantId(1))
            .await
            .pending_for_test()
            .await,
        ["d01"]
    );
}

#[tokio::test]
async fn dropping_an_index_returns_its_buffered_charge() {
    let (api, _store) = serve(MemoryStore::new(), limits(8 << 20, 64 << 20));
    assert_eq!(write(&api, 1, "d01", "durable").await.0, StatusCode::OK);
    for id in ["d02", "d03"] {
        assert_eq!(write(&api, 1, id, "batched").await.0, StatusCode::OK);
    }
    assert_eq!(buffered(&api, 1).await.pending, 2 * f());
    let (s, b, _) = send(&api, 1, "DELETE", "/v1/indexes/docs", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(buffered(&api, 1).await.pending, 0);
}

#[tokio::test]
async fn a_full_batched_write_is_refused_before_any_flush() {
    let (api, store) = serve(MemoryStore::new(), limits(f(), 2 * f() - 1));
    assert_eq!(write(&api, 1, "d01", "batched").await.0, StatusCode::OK);
    // It would pass the flush bound too, but a flush cannot make it fit: no request is spent.
    let at = totals(&store);
    let (s, b, retry) = write(&api, 1, "d02", "batched").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{b}");
    assert_eq!(retry.as_deref(), Some("1"));
    assert_eq!(totals(&store), at, "a refusal costs no request");
    assert_eq!(
        api.hold_engine_for_test(TenantId(1))
            .await
            .pending_for_test()
            .await,
        ["d01"]
    );
}

#[tokio::test]
async fn deletes_reserve_their_tombstones() {
    // Each delete is stored as a tombstone: its id and an 8-byte mark.
    let ids = |n: usize| (0..n).map(|i| format!("x{i:02}")).collect::<Vec<_>>();
    let cap = 10 * (3 + 8) - 1;
    let (api, store) = serve(MemoryStore::new(), limits(cap, cap));
    let delete = |n| json!({"durability": "batched", "deletes": ids(n)});
    let path = "/v1/indexes/docs/documents";
    let (s, b, _) = send(&api, 1, "PUT", path, &delete(10)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
    assert_eq!(totals(&store), [0; 4]);
    let (s, b, _) = send(&api, 1, "PUT", path, &delete(9)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(buffered(&api, 1).await.pending, 9 * (3 + 8));
}

/// A store whose first bundle PUT after [`Held::arm`] waits until [`Held::release`].
#[derive(Debug, Clone)]
struct Held {
    inner: MemoryStore,
    gate: Arc<Gate>,
}

#[derive(Debug)]
struct Gate {
    armed: std::sync::atomic::AtomicBool,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Held {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            gate: Arc::new(Gate {
                armed: std::sync::atomic::AtomicBool::new(false),
                arrived: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
            }),
        }
    }
    fn arm(&self) {
        self.gate
            .armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    async fn arrived(&self) {
        self.gate.arrived.notified().await;
    }
    fn release(&self) {
        self.gate.release.add_permits(1);
    }
    async fn hold(&self, key: &Key) {
        if key.as_str().ends_with(".bundle")
            && self
                .gate
                .armed
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.gate.arrived.notify_one();
            drop(self.gate.release.acquire().await.unwrap());
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Held {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.hold(key).await;
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.hold(key).await;
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test]
async fn a_running_write_is_charged_once() {
    let store = Held::new();
    let (api, _) = serve(store.clone(), limits(2 * f(), 2 * f()));
    store.arm();
    let a = tokio::spawn({
        let api = Arc::clone(&api);
        async move { write(&api, 1, "a01", "durable").await }
    });
    store.arrived().await;
    // Its rows are buffered and its PUT is in flight: the rows count, the reservation no more.
    assert_eq!(
        buffered(&api, 1).await,
        Buffered {
            pending: f(),
            reserved: 0,
            unfolded: 0
        }
    );
    let (s, b, _) = write(&api, 1, "b01", "batched").await;
    assert_eq!(s, StatusCode::OK, "it fits beside the running write: {b}");
    store.release();
    assert_eq!(a.await.unwrap().0, StatusCode::OK);
    assert_eq!(buffered(&api, 1).await.reserved, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_reservations_admit_one() {
    let store = Held::new();
    let cap = f() + f() / 2;
    let (api, _) = serve(store.clone(), limits(cap, cap));
    store.arm();
    // Each fits alone and the two do not fit together; both start at once.
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let racer = |id: &'static str| {
        let (api, start) = (Arc::clone(&api), Arc::clone(&start));
        tokio::spawn(async move {
            start.wait().await;
            (id, write(&api, 1, id, "durable").await)
        })
    };
    let (first, second) = (racer("r01"), racer("r02"));
    // The admitted one is held at its PUT, so the other settles first, refused.
    let (settled, admitted) = match futures_util::future::select(first, second).await {
        futures_util::future::Either::Left((done, other))
        | futures_util::future::Either::Right((done, other)) => (done.unwrap(), other),
    };
    let (loser, (s, b, retry)) = settled;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{loser}: {b}");
    assert_eq!(retry.as_deref(), Some("1"));
    store.arrived().await;
    store.release();
    let (winner, (s, b, _)) = admitted.await.unwrap();
    assert_eq!(s, StatusCode::OK, "{winner}: {b}");
    assert_eq!(ids(&api, 1).await, [winner]);
    assert_eq!(buffered(&api, 1).await.reserved, 0);
}
