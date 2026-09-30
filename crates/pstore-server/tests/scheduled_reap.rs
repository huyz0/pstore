//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A scheduled reap (M18): due from memory, so an idle tenant costs nothing, and bounded by
//! time, so what a reader of the last `age` could still need is kept.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_server::{Api, Config, GcPolicy, run_reaps, serve_folding};
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tower::ServiceExt;

const HOUR: Duration = Duration::from_secs(3600);

/// A memory store whose HEAD commits can be refused, shared by its clones.
#[derive(Debug, Clone, Default)]
struct Switch {
    inner: MemoryStore,
    refuse_commits: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl BlobStore for Switch {
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
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        if key.as_str().ends_with("/HEAD") && self.refuse_commits.load(Ordering::SeqCst) {
            return Err(CasError::Io("the store is down".to_owned()));
        }
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
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_suffix_as(key, n, class).await
    }
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_immutable(key, class).await
    }
}

struct World {
    switch: Switch,
    store: Accounted<Switch>,
    api: Arc<Api<Switch>>,
}

fn world() -> World {
    let switch = Switch::default();
    let store = Accounted::new(switch.clone());
    let api = Api::new(store.clone(), LaneId(1)).unwrap();
    World { switch, store, api }
}

impl World {
    fn requests(&self, tenant: u64) -> u64 {
        [OpClass::Read, OpClass::Write, OpClass::List]
            .into_iter()
            .map(|c| self.store.count(TenantId(u128::from(tenant)), c))
            .sum()
    }

    async fn bundles(&self) -> usize {
        self.switch
            .inner
            .list_unrestricted(&Key::new(String::new()))
            .await
            .unwrap()
            .iter()
            .filter(|k| k.as_str().ends_with(".bundle"))
            .count()
    }
}

async fn send(api: &Arc<Api<Switch>>, req: Request<Body>) -> (StatusCode, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn request(tenant: u64, method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

/// A durable write of `id` and a fold of it: one commit, which buries its bundle.
async fn commit(api: &Arc<Api<Switch>>, tenant: u64, id: &str) -> u64 {
    let body = json!({"durability": "durable", "documents": [{"id": id, "vector": [1.0, 0.5]}]});
    let (s, b) = send(
        api,
        request(tenant, "PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(api, request(tenant, "POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["epoch"].as_u64().unwrap()
}

async fn query(api: &Arc<Api<Switch>>, tenant: u64, as_of: Option<u64>) -> (StatusCode, Value) {
    let mut q = json!({"rank_by": ["id", "asc"], "top_k": 100});
    if let Some(e) = as_of {
        q["as_of"] = json!(e);
    }
    send(
        api,
        request(tenant, "POST", "/v1/indexes/docs/query", Some(&q)),
    )
    .await
}

fn policy(age: Duration) -> GcPolicy {
    GcPolicy {
        period: Duration::from_secs(1),
        age,
    }
}

#[tokio::test(start_paused = true)]
async fn a_tenant_is_reaped_once_its_commit_is_age_old_and_not_a_second_before() {
    let w = world();
    commit(&w.api, 70, "a").await;
    let buried = w.bundles().await;
    assert!(buried > 0);
    tokio::time::advance(HOUR - Duration::from_secs(1)).await;
    let before = w.requests(70);
    assert_eq!(w.api.reap_due(&policy(HOUR)).await, Default::default());
    assert_eq!(w.requests(70), before, "a tick not yet due made a request");
    assert_eq!(w.bundles().await, buried);
    tokio::time::advance(Duration::from_secs(1)).await;
    let tick = w.api.reap_due(&policy(HOUR)).await;
    assert_eq!(tick.reaped, 1, "{tick:?}");
    assert!(tick.objects > 0, "{tick:?}");
    assert_eq!(w.bundles().await, 0, "the buried bundle survived");
    // With nothing committed since, the tick an `age` later costs nothing.
    tokio::time::advance(HOUR).await;
    let before = w.requests(70);
    assert_eq!(w.api.reap_due(&policy(HOUR)).await, Default::default());
    assert_eq!(w.requests(70), before, "the reap made itself due");
}

#[tokio::test(start_paused = true)]
async fn an_idle_tenant_costs_nothing() {
    let w = world();
    // A tenant only read from, and one never touched.
    let (s, _) = query(&w.api, 71, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    tokio::time::advance(HOUR * 2).await;
    let before = w.requests(71) + w.requests(72);
    assert_eq!(w.api.reap_due(&policy(HOUR)).await, Default::default());
    assert_eq!(w.requests(71) + w.requests(72), before);
}

#[tokio::test(start_paused = true)]
async fn the_reap_keeps_what_the_last_age_can_read() {
    let w = world();
    commit(&w.api, 73, "a").await;
    let horizon = commit(&w.api, 73, "b").await;
    tokio::time::advance(HOUR).await;
    // Younger than `age`: its bundle is kept.
    let young = commit(&w.api, 73, "c").await;
    let tick = w.api.reap_due(&policy(HOUR)).await;
    assert_eq!(tick.reaped, 1);
    assert_eq!(w.bundles().await, 1, "the young commit's bundle was reaped");
    let live = query(&w.api, 73, None).await;
    assert_eq!(live.0, StatusCode::OK);
    assert_eq!(live.1["results"].as_array().unwrap().len(), 3);
    for (at, rows) in [(horizon, 2), (young, 3)] {
        let (s, b) = query(&w.api, 73, Some(at)).await;
        assert_eq!(s, StatusCode::OK, "as_of {at}: {b}");
        assert_eq!(b["results"].as_array().unwrap().len(), rows, "as_of {at}");
    }
    let (s, b) = query(&w.api, 73, Some(horizon - 1)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "time_travel_horizon");
}

#[tokio::test(start_paused = true)]
async fn a_failed_reap_backs_off_keeps_its_records_and_recovers() {
    let w = world();
    commit(&w.api, 74, "a").await;
    tokio::time::advance(HOUR).await;
    w.switch.refuse_commits.store(true, Ordering::SeqCst);
    let p = policy(HOUR);
    assert_eq!(w.api.reap_due(&p).await.failed, 1);
    tokio::time::advance(p.period / 2).await;
    let before = w.requests(74);
    assert_eq!(w.api.reap_due(&p).await.deferred, 1);
    assert_eq!(
        w.requests(74),
        before,
        "a reap inside its backoff was retried"
    );
    w.switch.refuse_commits.store(false, Ordering::SeqCst);
    tokio::time::advance(p.period).await;
    let tick = w.api.reap_due(&p).await;
    assert_eq!((tick.reaped, tick.failed), (1, 0), "{tick:?}");
    assert_eq!(w.bundles().await, 0);
}

#[tokio::test(start_paused = true)]
async fn the_loop_reaps_on_its_own_and_stops() {
    let w = world();
    commit(&w.api, 75, "a").await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let looping = tokio::spawn(run_reaps(Arc::clone(&w.api), policy(HOUR), async move {
        let _ = stopped.await;
    }));
    tokio::time::sleep(HOUR + Duration::from_secs(2)).await;
    assert_eq!(w.bundles().await, 0, "nothing reaped within age + period");
    stop.send(()).unwrap();
    looping.await.unwrap();
}

#[tokio::test]
async fn a_served_process_reaps_and_stops_with_its_signal() {
    let w = world();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let quick = GcPolicy {
        period: Duration::from_millis(20),
        age: Duration::from_millis(50),
    };
    let server = tokio::spawn(serve_folding(
        Arc::clone(&w.api),
        listener,
        async move {
            let _ = stopped.await;
        },
        None,
        Some(quick),
    ));
    commit(&w.api, 76, "a").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while w.bundles().await > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the served loop never reaped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    // Nothing reaps after `serve_folding` returned, even with a commit left due.
    commit(&w.api, 77, "b").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(w.bundles().await > 0, "a reap ran after the server stopped");
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

#[test]
fn the_reap_is_configured_by_name() {
    let c = Config::from_vars(vars(&[("PSTORE_LANE", "1")])).unwrap();
    assert_eq!(c.gc, Some(GcPolicy::default()));
    assert_eq!(
        GcPolicy::default(),
        GcPolicy {
            period: Duration::from_secs(1),
            age: HOUR
        }
    );
    let off = Config::from_vars(vars(&[("PSTORE_LANE", "1"), ("PSTORE_GC", "off")])).unwrap();
    assert_eq!(off.gc, None);
    let set = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_GC_PERIOD_MS", "250"),
        ("PSTORE_GC_AGE_S", "60"),
    ]))
    .unwrap();
    assert_eq!(
        set.gc,
        Some(GcPolicy {
            period: Duration::from_millis(250),
            age: Duration::from_secs(60)
        })
    );
    for (var, value) in [
        ("PSTORE_GC_PERIOD_MS", "0"),
        ("PSTORE_GC_AGE_S", "-1"),
        ("PSTORE_GC_AGE_S", "x"),
        ("PSTORE_GC", "on"),
    ] {
        let e = Config::from_vars(vars(&[("PSTORE_LANE", "1"), (var, value)])).unwrap_err();
        assert!(e.to_string().contains(var), "{var}={value}: {e}");
    }
    // `off` does not excuse a bad value beside it.
    let e = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_GC", "off"),
        ("PSTORE_GC_AGE_S", "0"),
    ]))
    .unwrap_err();
    assert!(e.to_string().contains("PSTORE_GC_AGE_S"), "{e}");
}

#[tokio::test(start_paused = true)]
async fn the_reap_backoff_doubles_to_its_cap() {
    let w = world();
    let p = GcPolicy {
        period: Duration::from_secs(1),
        age: Duration::from_secs(64),
    };
    commit(&w.api, 78, "a").await;
    tokio::time::advance(p.age).await;
    w.switch.refuse_commits.store(true, Ordering::SeqCst);
    // Doubling: attempts at 0, 1, 3, 7, 15, 31, 63 -- seven in the first 64 ticks, not 64.
    let mut failed = 0;
    for _ in 0..64 {
        failed += w.api.reap_due(&p).await.failed;
        tokio::time::advance(p.period).await;
    }
    assert!((5..=8).contains(&failed), "{failed} attempts in 64 ticks");
    // Capped at `age`: one a minute from then on, not ever rarer.
    let mut late = 0;
    for _ in 0..(64 * 4) {
        late += w.api.reap_due(&p).await.failed;
        tokio::time::advance(p.period).await;
    }
    assert!((3..=5).contains(&late), "{late} attempts in four ages");
}
