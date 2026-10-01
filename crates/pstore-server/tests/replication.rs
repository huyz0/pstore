//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Replication jobs over HTTP, and the workers that run them (M22.3).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_server::{Api, ReplicationPolicy, Worker};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const SRC: u64 = 71;
const DST: u64 = 72;

fn policy() -> ReplicationPolicy {
    ReplicationPolicy {
        scan: Duration::from_secs(10),
        ttl: Duration::from_secs(120),
        max: 64,
        period: Duration::from_secs(1),
        idle: Duration::from_secs(60),
        shards: 4,
    }
}

/// One store, any number of processes over it, and one remote store named "far".
struct World {
    acct: Accounted<MemoryStore>,
    far: Accounted<MemoryStore>,
}

impl World {
    fn new() -> Self {
        Self {
            acct: Accounted::new(MemoryStore::new()),
            far: Accounted::new(MemoryStore::new()),
        }
    }
    fn api(&self, lane: u64) -> A {
        self.api_with(lane, policy())
    }
    fn api_with(&self, lane: u64, policy: ReplicationPolicy) -> A {
        let api = Api::new(self.acct.clone(), LaneId(lane)).unwrap();
        let far: Arc<dyn BlobStore> = Arc::new(self.far.as_tenant(TenantId(0)));
        api.configure_replication(policy, BTreeMap::from([("far".to_owned(), far)]));
        api
    }
    /// A process serving the remote store's tenants.
    fn far_api(&self) -> A {
        Api::new(self.far.clone(), LaneId(50)).unwrap()
    }
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

async fn write(api: &A, tenant: u64, index: &str, ids: std::ops::Range<u32>) {
    let docs: Vec<Value> = ids
        .map(|i| json!({"id": format!("d{i:04}"), "vector": [f64::from(i).sin(), 1.0], "n": i}))
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

/// Every row of `index`, by id, or `None` when it does not exist.
async fn rows(api: &A, tenant: u64, index: &str) -> Option<Vec<String>> {
    let (s, b) = send(
        api,
        tenant,
        "POST",
        &format!("/v1/indexes/{index}/query"),
        &json!({"rank_by": ["id", "asc"], "top_k": 10_000}),
    )
    .await;
    if s == StatusCode::NOT_FOUND {
        return None;
    }
    assert_eq!(s, StatusCode::OK, "{b}");
    Some(
        b["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect(),
    )
}

async fn create(api: &A, tenant: u64, dest: &str, source: Value) -> (StatusCode, Value) {
    send(
        api,
        tenant,
        "PUT",
        &format!("/v1/indexes/{dest}/replication"),
        &json!({ "source": source }),
    )
    .await
}

async fn status(api: &A, tenant: u64, dest: &str) -> (StatusCode, Value) {
    send(
        api,
        tenant,
        "GET",
        &format!("/v1/indexes/{dest}/replication"),
        &Value::Null,
    )
    .await
}

/// Ticks every worker, then lets `step` of paused time pass, `n` times.
async fn run(workers: &mut [(A, Worker)], n: usize, step: Duration) {
    for _ in 0..n {
        for (api, w) in workers.iter_mut() {
            api.replication_tick(w).await;
        }
        tokio::time::advance(step).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_job_is_created_followed_paused_resumed_listed_and_cancelled() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..20).await;
    let (s, b) = create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["state"], "running", "{b}");
    assert_eq!(b["queued"], true, "{b}");

    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 2, Duration::from_secs(1)).await;
    assert_eq!(rows(&api, DST, "dst").await, rows(&api, SRC, "src").await);
    let (s, b) = status(&api, DST, "dst").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["applied_epoch"].as_u64().is_some(), "{b}");
    assert_eq!(b["rows"], 20, "{b}");
    assert!(b["segments"].as_u64().unwrap() >= 1, "{b}");
    assert_eq!(b["claim"]["owner"], 1, "{b}");
    assert!(b["last_commit_ms"].as_u64().is_some(), "{b}");
    assert_eq!(b["last_error"], Value::Null, "{b}");
    assert_eq!(b["source"]["tenant"], SRC.to_string(), "{b}");

    // Paused: the source moves, the replica does not, and nothing is queued.
    let (s, b) = send(
        &api,
        DST,
        "POST",
        "/v1/indexes/dst/replication/pause",
        &Value::Null,
    )
    .await;
    assert_eq!(
        (s, b["state"].as_str()),
        (StatusCode::OK, Some("paused")),
        "{b}"
    );
    assert_eq!(b["queued"], false, "{b}");
    write(&api, SRC, "src", 20..25).await;
    run(&mut workers, 5, Duration::from_secs(60)).await;
    assert_eq!(rows(&api, DST, "dst").await.unwrap().len(), 20);

    // Resumed: it catches up.
    let (s, b) = send(
        &api,
        DST,
        "POST",
        "/v1/indexes/dst/replication/resume",
        &Value::Null,
    )
    .await;
    assert_eq!(
        (s, b["state"].as_str()),
        (StatusCode::OK, Some("running")),
        "{b}"
    );
    assert_eq!(b["queued"], true, "{b}");
    run(&mut workers, 2, Duration::from_secs(1)).await;
    assert_eq!(rows(&api, DST, "dst").await.unwrap().len(), 25);

    // Many: a second job on the same tenant, and the list shows both.
    create(
        &api,
        DST,
        "dst2",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let (s, b) = send(&api, DST, "GET", "/v1/replications", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let names: BTreeSet<&str> = b["replications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["index"].as_str().unwrap())
        .collect();
    assert_eq!(names, BTreeSet::from(["dst", "dst2"]));

    // Cancelled: the index stays, and takes writes.
    let (s, b) = send(
        &api,
        DST,
        "DELETE",
        "/v1/indexes/dst/replication",
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["epoch"].as_u64().is_some(), "{b}");
    let (s, _) = status(&api, DST, "dst").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    write(&api, DST, "dst", 900..901).await;
    assert_eq!(rows(&api, DST, "dst").await.unwrap().len(), 26);
}

#[tokio::test(start_paused = true)]
async fn every_refusal_is_named() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..5).await;
    write(&api, DST, "taken", 0..5).await;
    let src = json!({"index": "src", "tenant": SRC.to_string()});
    let code = |b: &Value| b["error"]["code"].as_str().unwrap_or("").to_owned();
    for (dest, source, status, want) in [
        (
            "bad*name",
            src.clone(),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"index": "src", "store": "nowhere"}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"index": "nope", "tenant": SRC.to_string()}),
            StatusCode::NOT_FOUND,
            "source_not_found",
        ),
        ("taken", src.clone(), StatusCode::CONFLICT, "index_exists"),
        (
            "x",
            json!({"index": "x"}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"tenant": SRC.to_string()}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
    ] {
        let (s, b) = create(&api, DST, dest, source.clone()).await;
        assert_eq!(
            (s, code(&b)),
            (status, want.to_owned()),
            "{dest} {source}: {b}"
        );
    }
    let (s, _) = create(&api, DST, "dst", src.clone()).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, b) = create(&api, DST, "dst", src.clone()).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, "replication_exists")
    );
    // The replica refuses writes, a drop, and time travel.
    let (s, b) = send(
        &api,
        DST,
        "PUT",
        "/v1/indexes/dst/documents",
        &json!({"documents": [{"id": "x", "vector": [1.0, 0.0]}]}),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, "replica_read_only"),
        "{b}"
    );
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 1, Duration::from_secs(1)).await;
    let (s, b) = send(&api, DST, "DELETE", "/v1/indexes/dst", &Value::Null).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, "replication_active"),
        "{b}"
    );
    let (_, st) = status(&api, DST, "dst").await;
    let (s, b) = send(
        &api,
        DST,
        "POST",
        "/v1/indexes/dst/query",
        &json!({"rank_by": ["id", "asc"], "top_k": 5, "as_of": st["applied_epoch"]}),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, "replica_no_history"),
        "{b}"
    );
    for (method, uri) in [
        ("GET", "/v1/indexes/none/replication"),
        ("POST", "/v1/indexes/none/replication/pause"),
        ("POST", "/v1/indexes/none/replication/resume"),
        ("DELETE", "/v1/indexes/none/replication"),
    ] {
        let (s, b) = send(&api, DST, method, uri, &Value::Null).await;
        assert_eq!(
            (s, code(&b).as_str()),
            (StatusCode::NOT_FOUND, "replication_not_found"),
            "{method} {uri}: {b}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_list_costs_one_read() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..5).await;
    create(
        &api,
        DST,
        "a",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    create(
        &api,
        DST,
        "b",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let before = world.acct.count(TenantId(u128::from(DST)), OpClass::Read);
    let (s, b) = send(&api, DST, "GET", "/v1/replications", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["replications"].as_array().unwrap().len(), 2);
    assert_eq!(
        world.acct.count(TenantId(u128::from(DST)), OpClass::Read) - before,
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_status_call_repairs_a_lost_entry() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..5).await;
    create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    // A crash between a control call's HEAD CAS and its reconcile: the entry is gone.
    api.forget_replication_entry_for_test(TenantId(u128::from(DST)))
        .await;
    let (_, b) = status(&api, DST, "dst").await;
    // The status saw the disagreement, repaired it, and says so.
    assert_eq!(b["queued"], true, "{b}");
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 2, Duration::from_secs(1)).await;
    assert_eq!(rows(&api, DST, "dst").await.unwrap().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn a_remote_source_is_followed() {
    let world = World::new();
    let far = world.far_api();
    write(&far, SRC, "src", 0..12).await;
    let api = world.api(1);
    let (s, b) = create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string(), "store": "far"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 2, Duration::from_secs(1)).await;
    assert_eq!(rows(&api, DST, "dst").await, rows(&far, SRC, "src").await);
}

fn tenants() -> Vec<u64> {
    (100..110).collect()
}

async fn ten_jobs(world: &World, api: &A) {
    write(api, SRC, "src", 0..10).await;
    for t in tenants() {
        let (s, b) = create(
            api,
            t,
            "dst",
            json!({"index": "src", "tenant": SRC.to_string()}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }
    let _ = world;
}

/// Which lane holds each tenant's entry, read straight from the register.
async fn holders(api: &A) -> BTreeMap<u64, Option<u64>> {
    let mut out = BTreeMap::new();
    for t in tenants() {
        let (_, b) = status(api, t, "dst").await;
        out.insert(t, b["claim"]["owner"].as_u64());
    }
    out
}

#[tokio::test(start_paused = true)]
async fn three_workers_converge_and_share_the_work() {
    let world = World::new();
    // The jobs are created through a process with no worker: none is claimed at birth.
    let control = world.api(9);
    ten_jobs(&world, &control).await;
    let mut workers: Vec<(A, Worker)> =
        (1..=3).map(|l| (world.api(l), Worker::default())).collect();
    // Long enough for every shard to be scanned by someone: S·scan.
    run(&mut workers, 50, Duration::from_secs(1)).await;
    for t in tenants() {
        assert_eq!(
            rows(&control, t, "dst").await,
            rows(&control, SRC, "src").await,
            "tenant {t} did not converge"
        );
    }
    let held = holders(&control).await;
    assert!(held.values().all(|o| matches!(o, Some(1..=3))), "{held:?}");
    // One holder per tenant, by construction of the register; and the source moving on
    // reaches every replica through whichever worker holds it.
    write(&control, SRC, "src", 10..15).await;
    run(&mut workers, 70, Duration::from_secs(1)).await;
    for t in tenants() {
        assert_eq!(
            rows(&control, t, "dst").await.unwrap().len(),
            15,
            "tenant {t}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_dead_workers_tenants_move() {
    let world = World::new();
    let control = world.api(9);
    ten_jobs(&world, &control).await;
    // Worker 1 starts alone and takes everything.
    let mut first = vec![(world.api(1), Worker::default())];
    run(&mut first, 2, Duration::from_secs(1)).await;
    assert!(holders(&control).await.values().all(|o| *o == Some(1)));
    // It dies. Two others start; within ttl + S·scan every tenant is theirs.
    let mut rest: Vec<(A, Worker)> = (2..=3).map(|l| (world.api(l), Worker::default())).collect();
    let bound = policy().ttl + policy().scan * u32::from(policy().shards);
    let ticks = usize::try_from(bound.as_secs()).unwrap() + 2;
    run(&mut rest, ticks, Duration::from_secs(1)).await;
    let held = holders(&control).await;
    assert!(held.values().all(|o| matches!(o, Some(2 | 3))), "{held:?}");
    // Idle by now, so a change is seen within one `idle` interval.
    write(&control, SRC, "src", 10..13).await;
    let ticks = usize::try_from(policy().idle.as_secs()).unwrap() + 1;
    run(&mut rest, ticks, Duration::from_secs(1)).await;
    for t in tenants() {
        assert_eq!(
            rows(&control, t, "dst").await.unwrap().len(),
            13,
            "tenant {t}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_new_replication_on_a_held_tenant_syncs_within_a_renewal() {
    // Enough shards that no scan reaches the tenant's in the window: the renewal must.
    let world = World::new();
    let wide = ReplicationPolicy {
        shards: 16,
        ..policy()
    };
    let api = world.api_with(1, wide);
    write(&api, SRC, "src", 0..6).await;
    let control = world.api_with(9, wide);
    create(
        &control,
        DST,
        "a",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    // Held, synced, and idle long enough that the worker only polls the source.
    run(&mut workers, 200, Duration::from_secs(1)).await;
    create(
        &control,
        DST,
        "b",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let within = policy().ttl / 3 + policy().period;
    let ticks = usize::try_from(within.as_secs()).unwrap() + 1;
    run(&mut workers, ticks, Duration::from_secs(1)).await;
    assert_eq!(rows(&api, DST, "b").await.unwrap().len(), 6);
}

#[tokio::test(start_paused = true)]
async fn a_creator_with_a_worker_claims_its_job_at_once() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..6).await;
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 1, Duration::from_secs(1)).await;
    create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    // One tick: no scan has reached the shard, yet the job is held and synced.
    run(&mut workers, 1, Duration::from_secs(1)).await;
    assert_eq!(rows(&api, DST, "dst").await.unwrap().len(), 6);
}

#[tokio::test(start_paused = true)]
async fn a_worker_at_rest_costs_what_the_spec_says() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..6).await;
    for d in ["a", "b"] {
        create(
            &api,
            DST,
            d,
            json!({"index": "src", "tenant": SRC.to_string()}),
        )
        .await;
    }
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    // Settled: synced, idle at `idle`, renewing.
    run(&mut workers, 400, Duration::from_secs(1)).await;
    let system = TenantId(0);
    let dst = TenantId(u128::from(DST));
    let (r0, w0, d0, dw0) = (
        world.acct.count(system, OpClass::Read),
        world.acct.count(system, OpClass::Write),
        world.acct.count(dst, OpClass::Read),
        world.acct.count(dst, OpClass::Write),
    );
    let window = 600u64;
    run(
        &mut workers,
        usize::try_from(window).unwrap(),
        Duration::from_secs(1),
    )
    .await;
    let p = policy();
    // One shard read per scan; one CAS per held shard per ttl/3 (one tenant, one shard),
    // whose read is the CAS's own.
    let scans = window / p.scan.as_secs();
    let renewals = window / (p.ttl.as_secs() / 3);
    assert_eq!(
        world.acct.count(system, OpClass::Read) - r0,
        scans + renewals
    );
    assert_eq!(world.acct.count(system, OpClass::Write) - w0, renewals);
    // One read per distinct source per idle interval: both replicas share a source.
    assert_eq!(
        world.acct.count(dst, OpClass::Read) - d0,
        window / p.idle.as_secs()
    );
    // And no write for the tenant at all: no commit, no unchanged status note rewritten.
    assert_eq!(world.acct.count(dst, OpClass::Write), dw0);
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .chain([("PSTORE_LANE".to_owned(), "1".to_owned())])
        .collect();
    move |k| map.get(k).cloned()
}

#[test]
fn the_worker_and_its_sources_are_configured_from_the_environment() {
    use pstore_server::{Config, ConfigError, SourceConfig};
    // Unset: the worker runs with the default policy, and there are no sources.
    let c = Config::from_vars(vars(&[])).unwrap();
    assert_eq!(c.replication, Some(ReplicationPolicy::default()));
    assert!(c.sources.is_empty());
    assert_eq!(
        Config::from_vars(vars(&[("PSTORE_REPLICATION", "off")]))
            .unwrap()
            .replication,
        None
    );
    let c = Config::from_vars(vars(&[
        ("PSTORE_REPLICATION_SCAN_S", "5"),
        ("PSTORE_REPLICATION_TTL_S", "90"),
        ("PSTORE_REPLICATION_MAX", "7"),
        ("PSTORE_REPLICATION_IDLE_S", "30"),
        ("PSTORE_REPLICATION_SHARDS", "16"),
    ]))
    .unwrap();
    assert_eq!(
        c.replication,
        Some(ReplicationPolicy {
            scan: Duration::from_secs(5),
            ttl: Duration::from_secs(90),
            max: 7,
            idle: Duration::from_secs(30),
            shards: 16,
            ..ReplicationPolicy::default()
        })
    );
    for (k, v) in [
        ("PSTORE_REPLICATION", "maybe"),
        ("PSTORE_REPLICATION_SCAN_S", "0"),
        ("PSTORE_REPLICATION_TTL_S", "x"),
        ("PSTORE_REPLICATION_MAX", "0"),
        ("PSTORE_REPLICATION_SHARDS", "70000"),
    ] {
        assert!(
            matches!(
                Config::from_vars(vars(&[(k, v)])),
                Err(ConfigError::Replication(..))
            ),
            "{k}={v} was accepted"
        );
    }
    // Sources: named, each with an endpoint and a bucket, credentials both or neither.
    let c = Config::from_vars(vars(&[
        ("PSTORE_SOURCES", "eu, far"),
        ("PSTORE_SOURCE_EU_ENDPOINT", "https://s3.eu.example"),
        ("PSTORE_SOURCE_EU_BUCKET", "data"),
        ("PSTORE_SOURCE_EU_ACCESS_KEY", "k"),
        ("PSTORE_SOURCE_EU_SECRET_KEY", "s"),
        ("PSTORE_SOURCE_EU_REGION", "eu-west-1"),
        ("PSTORE_SOURCE_FAR_ENDPOINT", "http://far:9000"),
        ("PSTORE_SOURCE_FAR_BUCKET", "other"),
    ]))
    .unwrap();
    assert_eq!(
        c.sources,
        vec![
            SourceConfig {
                name: "eu".to_owned(),
                endpoint: "https://s3.eu.example".to_owned(),
                bucket: "data".to_owned(),
                credentials: Some(("k".to_owned(), "s".to_owned())),
                region: "eu-west-1".to_owned(),
            },
            SourceConfig {
                name: "far".to_owned(),
                endpoint: "http://far:9000".to_owned(),
                bucket: "other".to_owned(),
                credentials: None,
                region: "us-east-1".to_owned(),
            },
        ]
    );
    for bad in [
        vec![("PSTORE_SOURCES", "eu")],
        vec![
            ("PSTORE_SOURCES", "eu"),
            ("PSTORE_SOURCE_EU_ENDPOINT", "https://x"),
        ],
        vec![
            ("PSTORE_SOURCES", "eu"),
            ("PSTORE_SOURCE_EU_ENDPOINT", "https://x"),
            ("PSTORE_SOURCE_EU_BUCKET", "b"),
            ("PSTORE_SOURCE_EU_ACCESS_KEY", "k"),
        ],
        vec![("PSTORE_SOURCES", "e u")],
        vec![
            ("PSTORE_SOURCES", "eu,eu"),
            ("PSTORE_SOURCE_EU_ENDPOINT", "https://x"),
            ("PSTORE_SOURCE_EU_BUCKET", "b"),
        ],
    ] {
        assert!(
            matches!(Config::from_vars(vars(&bad)), Err(ConfigError::Source(..))),
            "{bad:?} was accepted"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_paused_job_costs_nothing_once_its_claim_is_dropped() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..6).await;
    create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 5, Duration::from_secs(1)).await;
    send(
        &api,
        DST,
        "POST",
        "/v1/indexes/dst/replication/pause",
        &Value::Null,
    )
    .await;
    // One renewal drops the tenant; after that, nothing.
    let renew = usize::try_from((policy().ttl / 3).as_secs()).unwrap() + 1;
    run(&mut workers, renew, Duration::from_secs(1)).await;
    let dst = TenantId(u128::from(DST));
    let before = world.acct.count(dst, OpClass::Read);
    run(&mut workers, 600, Duration::from_secs(1)).await;
    assert_eq!(world.acct.count(dst, OpClass::Read), before);
}

#[tokio::test(start_paused = true)]
async fn a_worker_without_the_named_store_says_why() {
    let world = World::new();
    let far = world.far_api();
    write(&far, SRC, "src", 0..4).await;
    // Created through a process that has "far"; worked by one that does not.
    let control = world.api(9);
    create(
        &control,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string(), "store": "far"}),
    )
    .await;
    let bare = Api::new(world.acct.clone(), LaneId(1)).unwrap();
    bare.configure_replication(policy(), BTreeMap::new());
    let mut workers = vec![(Arc::clone(&bare), Worker::default())];
    let ticks = usize::try_from((policy().ttl / 3).as_secs()).unwrap() + 2;
    run(&mut workers, ticks, Duration::from_secs(1)).await;
    let (_, b) = status(&control, DST, "dst").await;
    assert!(
        b["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown_store"),
        "{b}"
    );
    assert!(b["last_error"].as_str().unwrap().len() <= 256);
}

#[tokio::test(start_paused = true)]
async fn the_last_cancel_takes_the_status_notes_with_it() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..4).await;
    create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 2, Duration::from_secs(1)).await;
    let key = pstore_blob::Key::new(format!("{:04x}/tnt/{}/REPLSTATUS", DST as u16, DST));
    let probe = world.acct.as_tenant(TenantId(0));
    assert!(probe.get(&key).await.is_ok(), "no notes were written");
    send(
        &api,
        DST,
        "DELETE",
        "/v1/indexes/dst/replication",
        &Value::Null,
    )
    .await;
    assert!(
        probe.get(&key).await.is_err(),
        "the notes outlived the last replication"
    );
}

#[tokio::test]
async fn a_served_process_runs_its_worker() {
    let world = World::new();
    let api = Api::new(world.acct.clone(), LaneId(1)).unwrap();
    api.configure_replication(
        ReplicationPolicy {
            period: Duration::from_millis(20),
            idle: Duration::from_millis(40),
            ..policy()
        },
        BTreeMap::new(),
    );
    write(&api, SRC, "src", 0..4).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(pstore_server::serve_folding(
        Arc::clone(&api),
        listener,
        async move {
            let _ = rx.await;
        },
        None,
        None,
        true,
    ));
    create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while rows(&api, DST, "dst").await.map_or(0, |r| r.len()) < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the worker never synced"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = tx.send(());
    server.await.unwrap().unwrap();
}

const D: TenantId = TenantId(DST as u128);

#[tokio::test(start_paused = true)]
async fn every_control_call_leaves_the_register_right_by_itself() {
    // Read straight from the register: a status call would repair what a control left wrong.
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..4).await;
    let src = json!({"index": "src", "tenant": SRC.to_string()});
    create(&api, DST, "dst", src).await;
    assert!(api.replication_entry_for_test(D).await.is_some(), "create");
    send(
        &api,
        DST,
        "POST",
        "/v1/indexes/dst/replication/pause",
        &Value::Null,
    )
    .await;
    assert!(api.replication_entry_for_test(D).await.is_none(), "pause");
    send(
        &api,
        DST,
        "POST",
        "/v1/indexes/dst/replication/resume",
        &Value::Null,
    )
    .await;
    assert!(api.replication_entry_for_test(D).await.is_some(), "resume");
    send(
        &api,
        DST,
        "DELETE",
        "/v1/indexes/dst/replication",
        &Value::Null,
    )
    .await;
    assert!(api.replication_entry_for_test(D).await.is_none(), "cancel");
}

#[tokio::test(start_paused = true)]
async fn a_full_worker_does_not_claim_at_creation() {
    let world = World::new();
    let one = ReplicationPolicy { max: 1, ..policy() };
    let api = world.api_with(1, one);
    write(&api, SRC, "src", 0..4).await;
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 1, Duration::from_secs(1)).await;
    let src = json!({"index": "src", "tenant": SRC.to_string()});
    create(&api, 100, "dst", src.clone()).await;
    run(&mut workers, 1, Duration::from_secs(1)).await;
    create(&api, 101, "dst", src).await;
    let first = api.replication_entry_for_test(TenantId(100)).await.unwrap();
    let second = api.replication_entry_for_test(TenantId(101)).await.unwrap();
    assert_eq!((first.1, second.1), (Some(1), None));
}

#[tokio::test(start_paused = true)]
async fn after_a_commit_the_next_change_is_a_period_away() {
    let world = World::new();
    let api = world.api(1);
    write(&api, SRC, "src", 0..4).await;
    create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string()}),
    )
    .await;
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    // Idle: the interval has grown to `idle`.
    run(&mut workers, 300, Duration::from_secs(1)).await;
    write(&api, SRC, "src", 4..6).await;
    // Ticked until the commit lands -- within `idle` -- and no further, so nothing idles
    // the interval back up.
    let mut waited = 0;
    while rows(&api, DST, "dst").await.unwrap().len() < 6 {
        assert!(
            waited <= policy().idle.as_secs(),
            "the change was never seen"
        );
        run(&mut workers, 1, Duration::from_secs(1)).await;
        waited += 1;
    }
    // It just committed, so it is back at `period`: the next change is seen at once.
    write(&api, SRC, "src", 6..8).await;
    run(&mut workers, 3, Duration::from_secs(1)).await;
    assert_eq!(rows(&api, DST, "dst").await.unwrap().len(), 8);
}

#[tokio::test(start_paused = true)]
async fn a_worker_removes_an_entry_with_nothing_running() {
    let world = World::new();
    let api = world.api(1);
    api.plant_replication_entry_for_test(D).await;
    assert!(api.replication_entry_for_test(D).await.is_some());
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 3, Duration::from_secs(1)).await;
    assert!(api.replication_entry_for_test(D).await.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_remote_sources_reads_are_billed_to_the_replicas_tenant() {
    let world = World::new();
    let far = world.far_api();
    write(&far, SRC, "src", 0..6).await;
    let api = world.api(1);
    let before = api.source_reads_for_test("far", D);
    create(
        &api,
        DST,
        "dst",
        json!({"index": "src", "tenant": SRC.to_string(), "store": "far"}),
    )
    .await;
    let mut workers = vec![(Arc::clone(&api), Worker::default())];
    run(&mut workers, 2, Duration::from_secs(1)).await;
    assert!(
        api.source_reads_for_test("far", D) > before,
        "nothing billed to the tenant"
    );
    assert_eq!(api.source_reads_for_test("far", TenantId(0)), 0);
}
