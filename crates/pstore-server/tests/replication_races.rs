//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Replication control calls interleaved with a worker's tick, and with a register that
//! cannot be written (M22.3). Over a store that yields before every operation, so two
//! tasks on one runtime interleave at each request as they would against a real bucket.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition,
    PutOutcome,
};
use pstore_server::{Api, ReplicationPolicy, Worker};
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tower::ServiceExt;

/// A memory store that yields before every operation, and refuses conditional writes to the
/// register while `refuse_register_cas` is set.
#[derive(Debug, Clone, Default)]
struct Yielding {
    inner: Arc<MemoryStore>,
    refuse_register_cas: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl BlobStore for Yielding {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        tokio::task::yield_now().await;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        tokio::task::yield_now().await;
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        tokio::task::yield_now().await;
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        tokio::task::yield_now().await;
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        tokio::task::yield_now().await;
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        tokio::task::yield_now().await;
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        tokio::task::yield_now().await;
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        tokio::task::yield_now().await;
        if self.refuse_register_cas.load(Ordering::SeqCst) && key.as_str().contains("/jobs/") {
            return Err(CasError::Io("injected outage".to_owned()));
        }
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        tokio::task::yield_now().await;
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

type A = Arc<Api<Yielding>>;

const SRC: u64 = 71;

fn policy(max: usize) -> ReplicationPolicy {
    ReplicationPolicy {
        scan: Duration::from_secs(10),
        ttl: Duration::from_secs(120),
        max,
        period: Duration::from_secs(1),
        idle: Duration::from_secs(60),
        shards: 4,
    }
}

fn api(acct: &Accounted<Yielding>, lane: u64, max: usize) -> A {
    let api = Api::new(acct.clone(), LaneId(lane)).unwrap();
    api.configure_replication(policy(max), BTreeMap::new());
    api
}

async fn send(api: &A, tenant: u64, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn write(api: &A, tenant: u64, index: &str, ids: Range<u32>) {
    let docs: Vec<Value> = ids
        .map(|i| json!({"id": format!("d{i:04}"), "vector": [f64::from(i).sin(), 1.0]}))
        .collect();
    let (s, b) = send(
        api,
        tenant,
        "PUT",
        &format!("/v1/indexes/{index}/documents"),
        &json!({"durability": "durable", "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(api, tenant, "POST", "/v1/admin/fold", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn create(api: &A, tenant: u64) -> StatusCode {
    let body = json!({"source": {"index": "src", "tenant": SRC.to_string()}});
    send(api, tenant, "PUT", "/v1/indexes/dst/replication", &body)
        .await
        .0
}

async fn claimed_by(api: &A, tenant: u64) -> Option<u64> {
    api.replication_entry_for_test(TenantId(u128::from(tenant)))
        .await
        .and_then(|(_, owner)| owner)
}

#[tokio::test(start_paused = true)]
async fn a_claim_made_during_a_tick_still_counts_against_max() {
    // A create claims for the worker while a tick is syncing: the slot it reserved is still
    // reserved when the tick ends and recounts what it holds, so the next create, at max,
    // is not claimed past it.
    let acct = Accounted::new(Yielding::default());
    let api = api(&acct, 1, 2);
    write(&api, SRC, "src", 0..20).await;
    assert_eq!(create(&api, 100).await, StatusCode::CREATED);
    let mut w = Worker::default();
    api.replication_tick(&mut w).await;
    assert_eq!(claimed_by(&api, 100).await, Some(1));
    // Something for the next tick to copy, so it is still syncing when the create lands.
    write(&api, SRC, "src", 20..40).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let (tick, made) = tokio::join!(api.replication_tick(&mut w), create(&api, 101));
    assert_eq!(made, StatusCode::CREATED);
    assert_eq!(tick.committed, 1, "the tick was not syncing: {tick:?}");
    assert_eq!(claimed_by(&api, 101).await, Some(1));
    // Held 100, reserved for 101: full. A third create is left for a scan.
    assert_eq!(create(&api, 102).await, StatusCode::CREATED);
    assert_eq!(claimed_by(&api, 102).await, None);
}

#[tokio::test(start_paused = true)]
async fn a_control_call_whose_register_write_fails_still_answers() {
    // The HEAD CAS took; the register cannot be written. The change happened, so the call
    // answers with it -- a stale entry the status shows and a GET repairs -- never an error.
    let store = Yielding::default();
    let acct = Accounted::new(store.clone());
    let api = api(&acct, 1, 64);
    write(&api, SRC, "src", 0..5).await;
    assert_eq!(create(&api, 100).await, StatusCode::CREATED);
    store.refuse_register_cas.store(true, Ordering::SeqCst);
    let (status, body) = send(
        &api,
        100,
        "POST",
        "/v1/indexes/dst/replication/pause",
        &Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "paused", "{body}");
    // The entry outlived the pause: the next GET, with the register back, removes it.
    assert!(
        api.replication_entry_for_test(TenantId(100))
            .await
            .is_some()
    );
    store.refuse_register_cas.store(false, Ordering::SeqCst);
    let (status, _) = send(
        &api,
        100,
        "GET",
        "/v1/indexes/dst/replication",
        &Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        api.replication_entry_for_test(TenantId(100))
            .await
            .is_none()
    );
}

async fn ticks(api: &A, w: &mut Worker, n: usize) {
    for _ in 0..n {
        tokio::time::advance(Duration::from_secs(1)).await;
        api.replication_tick(w).await;
    }
}

#[tokio::test(start_paused = true)]
async fn every_slot_let_go_is_given_back() {
    // At max 1, each way a held tenant is let go must free the slot, or the worker stops
    // taking work it has room for.
    let acct = Accounted::new(Yielding::default());
    let api = api(&acct, 1, 1);
    write(&api, SRC, "src", 0..5).await;
    let mut w = Worker::default();
    api.replication_tick(&mut w).await;

    // An entry with nothing running in HEAD (a crash between a cancel's HEAD CAS and its
    // reconcile): claimed by a scan, found empty at its first sync, and dropped there.
    api.plant_replication_entry_for_test(TenantId(100)).await;
    ticks(&api, &mut w, 45).await;
    assert!(
        api.replication_entry_for_test(TenantId(100))
            .await
            .is_none()
    );
    assert_eq!(create(&api, 101).await, StatusCode::CREATED);
    assert_eq!(
        claimed_by(&api, 101).await,
        Some(1),
        "an empty tenant kept its slot"
    );

    // Its entry gone at a renewal: dropped there.
    ticks(&api, &mut w, 2).await;
    api.forget_replication_entry_for_test(TenantId(101)).await;
    ticks(&api, &mut w, 41).await;
    assert_eq!(create(&api, 102).await, StatusCode::CREATED);
    assert_eq!(
        claimed_by(&api, 102).await,
        Some(1),
        "a lost tenant kept its slot"
    );
}

#[tokio::test(start_paused = true)]
async fn a_control_call_on_a_held_tenant_gives_its_reservation_back() {
    let acct = Accounted::new(Yielding::default());
    let api = api(&acct, 1, 2);
    write(&api, SRC, "src", 0..5).await;
    let mut w = Worker::default();
    api.replication_tick(&mut w).await;
    assert_eq!(create(&api, 100).await, StatusCode::CREATED);
    ticks(&api, &mut w, 2).await;
    // A second replication on the tenant it holds: a slot reserved, handed over, and given
    // back at adoption, since the tenant is held already.
    let body = json!({"source": {"index": "src", "tenant": SRC.to_string()}});
    let (s, b) = send(&api, 100, "PUT", "/v1/indexes/dst2/replication", &body).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    ticks(&api, &mut w, 2).await;
    assert_eq!(create(&api, 101).await, StatusCode::CREATED);
    assert_eq!(
        claimed_by(&api, 101).await,
        Some(1),
        "the reservation leaked"
    );
}

#[tokio::test(start_paused = true)]
async fn a_restart_racing_a_create_holds_no_more_than_max() {
    // Three claims of this lane's from before a restart at max 3, taken back by the start
    // scan while a create reserves a slot: the scan counts the reservation.
    let acct = Accounted::new(Yielding::default());
    let before = api(&acct, 1, 64);
    write(&before, SRC, "src", 0..5).await;
    for t in 100..103 {
        assert_eq!(create(&before, t).await, StatusCode::CREATED);
    }
    before.replication_tick(&mut Worker::default()).await;
    let after = api(&acct, 1, 3);
    let mut w = Worker::default();
    // One worker per process, as served: its first tick -- polled first, so the worker is on
    // before the create reserves -- is the one that races.
    let (t1, made) = tokio::join!(after.replication_tick(&mut w), create(&after, 103));
    assert_eq!(made, StatusCode::CREATED);
    let tick = after.replication_tick(&mut w).await;
    // The create's reservation landed inside the start scan: it was claimed, and the scan
    // took back only what was left.
    assert_eq!(claimed_by(&after, 103).await, Some(1));
    assert_eq!(t1.held, 2, "{t1:?}");
    assert!(tick.held <= 3, "{tick:?}");
}

#[tokio::test(start_paused = true)]
async fn an_abandoned_control_call_gives_its_reservation_back() {
    // A client that goes away drops its request mid-flight: at whatever point that lands --
    // here, after each number of store operations in turn -- the slot a create reserved for
    // the worker is given back, or the worker would fill with slots nobody holds.
    let acct = Accounted::new(Yielding::default());
    let api = api(&acct, 1, 1);
    write(&api, SRC, "src", 0..5).await;
    api.replication_tick(&mut Worker::default()).await;
    for (n, tenant) in (1..40).zip(200u64..) {
        let a = Arc::clone(&api);
        let task = tokio::spawn(async move { create(&a, tenant).await });
        for _ in 0..n {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        // Whatever the abort left -- a replication that landed, an entry -- is taken away.
        let _ = send(
            &api,
            tenant,
            "DELETE",
            "/v1/indexes/dst/replication",
            &Value::Null,
        )
        .await;
        api.forget_replication_entry_for_test(TenantId(u128::from(tenant)))
            .await;
    }
    // A create aborted after handing its tenant over has used its slot, as it should: the
    // worker takes the tenant and lets it go at the renewal that finds its entry gone.
    let mut w = Worker::default();
    ticks(&api, &mut w, 45).await;
    assert_eq!(create(&api, 300).await, StatusCode::CREATED);
    assert_eq!(
        claimed_by(&api, 300).await,
        Some(1),
        "an abandoned create kept its slot"
    );
}
