//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The server's read cache (M20): a restart is not a flush, a hit is not billed, an uncached
//! server is unchanged, and the cache is configured by name.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_cache::{CacheCore, DiskConfig, DiskState};
use pstore_server::{Api, CacheConfig, Config, ConfigError, open_cache, store_identity};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: &str = "31";

/// A fresh directory, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pstore-server-cache-{}-{name}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn core(dir: &Dir) -> Arc<CacheCore> {
    Arc::new(CacheCore::open(64 << 20, DiskConfig::new(&dir.0, 1, 64 << 20, "test-store")).await)
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT)
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

/// Two folded segments of 50 rows each, written through `api`.
async fn seed(api: &A) {
    for part in 0..2 {
        let docs: Vec<Value> = (0..50)
            .map(|i| {
                let x = f64::from(part * 50 + i);
                json!({"id": format!("d{part}-{i}"), "vector": [x.sin(), x.cos()],
                       "attributes": {"n": i}})
            })
            .collect();
        let (s, b) = send(
            api,
            "PUT",
            "/v1/indexes/docs/documents",
            &json!({"durability": "durable", "documents": docs}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        api.fold_due(&pstore_server::FoldPolicy {
            period: std::time::Duration::from_secs(1),
            age: std::time::Duration::ZERO,
            bytes: 1,
        })
        .await;
    }
}

fn queries() -> Vec<Value> {
    queries_and_warm_cost()
        .into_iter()
        .map(|(q, _)| q)
        .collect()
}

/// Each query, and what it costs when every cacheable read hits: HEAD, plus -- for a vector
/// query -- one read per segment below `EXACT_SCAN_THRESHOLD`, whose centroid table is
/// absent. A 404 is not an object, so no cache keeps it (BACKLOG row 46).
fn queries_and_warm_cost() -> Vec<(Value, u64)> {
    let segments = 2;
    vec![
        (json!({"vector": [1.0, 0.0], "top_k": 10}), 1 + segments),
        (json!({"rank_by": ["id", "asc"], "top_k": 100}), 1),
        (
            json!({"vector": [0.0, 1.0], "top_k": 5, "filters": ["n", "Gt", 10]}),
            1 + segments,
        ),
    ]
}

async fn query(api: &A, q: &Value) -> (Vec<String>, u64) {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    let ids = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    (ids, b["meta"]["cost"]["blob_reads"].as_u64().unwrap())
}

#[tokio::test]
async fn a_restarted_server_is_not_a_flush() {
    let dir = Dir::new("restart");
    let store = MemoryStore::new();
    let first = core(&dir).await;
    let api =
        Api::with_cache(Accounted::new(store.clone()), LaneId(1), Arc::clone(&first)).unwrap();
    seed(&api).await;
    let mut before = Vec::new();
    for q in queries() {
        before.push(query(&api, &q).await.0);
    }
    first.close().await;

    let second = core(&dir).await;
    let api = Api::with_cache(Accounted::new(store), LaneId(1), Arc::clone(&second)).unwrap();
    for ((q, cost), want) in queries_and_warm_cost().iter().zip(before) {
        let (got, reads) = query(&api, q).await;
        assert_eq!(got, want, "{q}");
        assert_eq!(
            reads, *cost,
            "a restarted server read a cacheable object for {q}"
        );
    }
    second.close().await;
}

#[tokio::test]
async fn cached_answers_equal_uncached_ones() {
    let dir = Dir::new("equal");
    let store = MemoryStore::new();
    let plain = Api::new(Accounted::new(store.clone()), LaneId(2)).unwrap();
    seed(&plain).await;
    let c = core(&dir).await;
    let cached = Api::with_cache(Accounted::new(store.clone()), LaneId(1), Arc::clone(&c)).unwrap();
    for q in queries() {
        let want = query(&plain, &q).await.0;
        assert_eq!(query(&cached, &q).await.0, want, "cold {q}");
        assert_eq!(query(&cached, &q).await.0, want, "warm {q}");
    }
    c.close().await;
    let c = core(&dir).await;
    let cached = Api::with_cache(Accounted::new(store), LaneId(1), Arc::clone(&c)).unwrap();
    for q in queries() {
        assert_eq!(
            query(&cached, &q).await.0,
            query(&plain, &q).await.0,
            "reopened {q}"
        );
    }
    c.close().await;
}

#[tokio::test]
async fn a_hit_is_not_billed() {
    // A fresh cache per query, so each one's cold read is its own.
    for (q, cost) in queries_and_warm_cost() {
        let dir = Dir::new("billed");
        let c = core(&dir).await;
        let api = Api::with_cache(
            Accounted::new(MemoryStore::new()),
            LaneId(1),
            Arc::clone(&c),
        )
        .unwrap();
        seed(&api).await;
        let (_, cold) = query(&api, &q).await;
        let (_, warm) = query(&api, &q).await;
        assert!(cold > cost, "{q} read nothing cacheable cold");
        assert_eq!(warm, cost, "{q}: a hit was billed");
        c.close().await;
    }
}

#[tokio::test]
async fn the_uncached_api_repeats_its_reads() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    seed(&api).await;
    for q in queries() {
        let (_, first) = query(&api, &q).await;
        let (_, second) = query(&api, &q).await;
        assert!(first > 1, "{q}");
        assert_eq!(
            second, first,
            "{q}: an uncached server read less the second time"
        );
    }
}

#[tokio::test]
async fn a_server_over_a_bypassed_cache_serves() {
    let dir = Dir::new("bypassed");
    std::fs::write(&dir.0, b"a file").unwrap();
    let c = core(&dir).await;
    assert!(matches!(c.disk_state(), DiskState::Bypassed(_)));
    let api = Api::with_cache(Accounted::new(MemoryStore::new()), LaneId(1), c).unwrap();
    seed(&api).await;
    assert_eq!(query(&api, &queries()[0]).await.0.len(), 10);
    let _ = std::fs::remove_file(&dir.0);
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .chain([("PSTORE_LANE".to_owned(), "1".to_owned())])
        .collect();
    move |k| map.get(k).cloned()
}

const S3: [(&str, &str); 2] = [
    ("PSTORE_BACKEND", "s3"),
    ("PSTORE_S3_ENDPOINT", "http://minio:9000"),
];

#[test]
fn the_cache_is_configured_by_name() {
    // Absent: uncached, as before M20.
    assert!(Config::from_vars(vars(&S3)).unwrap().cache.is_none());
    // The directory alone: the defaults.
    let mut on = S3.to_vec();
    on.push(("PSTORE_CACHE_DIR", "/var/cache/pstore"));
    let c = Config::from_vars(vars(&on)).unwrap().cache.unwrap();
    assert_eq!(
        c,
        CacheConfig {
            dir: PathBuf::from("/var/cache/pstore"),
            ram: 256 << 20,
            disk: 4 << 30,
        }
    );
    let mut sized = on.clone();
    sized.push(("PSTORE_CACHE_RAM_BYTES", "1024"));
    sized.push(("PSTORE_CACHE_DISK_BYTES", "4096"));
    let c = Config::from_vars(vars(&sized)).unwrap().cache.unwrap();
    assert_eq!((c.ram, c.disk), (1024, 4096));
}

#[test]
fn a_bad_cache_setting_is_refused_by_name() {
    for var in ["PSTORE_CACHE_RAM_BYTES", "PSTORE_CACHE_DISK_BYTES"] {
        for bad in ["0", "-1", "lots"] {
            let mut v = S3.to_vec();
            v.push(("PSTORE_CACHE_DIR", "/c"));
            v.push((var, bad));
            let e = Config::from_vars(vars(&v)).unwrap_err();
            assert!(e.to_string().contains(var), "{var}={bad}: {e}");
            assert!(matches!(e, ConfigError::Cache(..)), "{e:?}");
        }
        // A size with no directory is a setting that would do nothing.
        let mut v = S3.to_vec();
        v.push((var, "1024"));
        let e = Config::from_vars(vars(&v)).unwrap_err();
        assert!(e.to_string().contains(var), "{e}");
        assert!(e.to_string().contains("PSTORE_CACHE_DIR"), "{e}");
    }
    // A memory store is new and empty every start, so its keys repeat with other bytes.
    let e = Config::from_vars(vars(&[("PSTORE_CACHE_DIR", "/c")])).unwrap_err();
    assert!(e.to_string().contains("PSTORE_CACHE_DIR"), "{e}");
    assert!(e.to_string().contains("memory"), "{e}");
}

#[tokio::test]
async fn the_store_id_is_created_once_and_read_back() {
    let store = Accounted::new(MemoryStore::new());
    let first = store_identity(&store, "http://minio:9000", "b")
        .await
        .unwrap();
    let reads = |s: &Accounted<MemoryStore>| s.count(TenantId(0), OpClass::Read);
    let r = reads(&store);
    let again = store_identity(&store, "http://minio:9000", "b")
        .await
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(
        reads(&store) - r,
        1,
        "reading the id back took more than one GET"
    );
    assert!(
        first.contains("http://minio:9000") && first.contains(" b "),
        "{first}"
    );
    // A bucket recreated empty gets a new id, so its old entries are never served.
    let fresh = Accounted::new(MemoryStore::new());
    let other = store_identity(&fresh, "http://minio:9000", "b")
        .await
        .unwrap();
    assert_ne!(other, first);
    // And the id lives in the store, where every node on it finds the same one.
    let raw = store
        .as_tenant(TenantId(0))
        .get(&Key::new("_pstore/store-id"))
        .await
        .unwrap();
    assert!(
        first.ends_with(std::str::from_utf8(&raw).unwrap()),
        "{first}"
    );
}

#[tokio::test]
async fn open_cache_opens_what_the_config_names() {
    let store = Accounted::new(MemoryStore::new());
    let mut vars_on = S3.to_vec();
    let config = Config::from_vars(vars(&vars_on)).unwrap();
    assert!(open_cache(&store, &config).await.unwrap().is_none());
    let dir = Dir::new("open");
    let path = dir.0.to_str().unwrap().to_owned();
    vars_on.push(("PSTORE_CACHE_DIR", &path));
    vars_on.push(("PSTORE_CACHE_DISK_BYTES", "67108864"));
    let config = Config::from_vars(vars(&vars_on)).unwrap();
    let c = open_cache(&store, &config).await.unwrap().unwrap();
    assert_eq!(*c.disk_state(), DiskState::Open);
    c.close().await;
    // In this lane's directory, recording this store.
    let recorded = std::fs::read_to_string(dir.0.join("lane-1").join("identity")).unwrap();
    let identity = store_identity(&store, "http://minio:9000", "pstore")
        .await
        .unwrap();
    assert!(recorded.ends_with(&identity), "{recorded}");
}

/// A store whose first GET of the store id misses, as it does for a node that read it just
/// before another node created it.
#[derive(Clone, Default)]
struct Raced {
    inner: MemoryStore,
    missed: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl BlobStore for Raced {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        if key.as_str() == "_pstore/store-id" && !self.missed.swap(true, Ordering::SeqCst) {
            return Err(pstore_blob::BlobError::NotFound(key.as_str().to_owned()));
        }
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn head(&self, key: &Key) -> Result<u64, pstore_blob::BlobError> {
        self.inner.head(key).await
    }
    async fn put(
        &self,
        key: &Key,
        body: bytes::Bytes,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: bytes::Bytes,
        pre: pstore_blob::Precondition,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn get_with_tag(
        &self,
        key: &Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(
        &self,
        key: &Key,
    ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
        self.inner.get_tag(key).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), pstore_blob::BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, pstore_blob::BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test]
async fn a_node_that_loses_the_id_race_takes_the_winners() {
    let raced = Raced::default();
    raced
        .inner
        .put(&Key::new("_pstore/store-id"), bytes::Bytes::from("winner"))
        .await
        .unwrap();
    let store = Accounted::new(raced);
    let id = store_identity(&store, "e", "b").await.unwrap();
    assert_eq!(id, "s3 e b winner");
}
