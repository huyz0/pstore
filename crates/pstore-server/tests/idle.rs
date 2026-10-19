//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What a node holds for an index it is not serving (M106, M6i criterion 7's node half).
//!
//! A writer seeds the store; a second `Api`, the serving node, answers the same queries on one
//! index in every arm. The arms differ only in what else the store holds, so anything the node
//! read or kept because of it shows as a difference.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition,
    PutOutcome,
};
use pstore_cache::{CacheCore, Caching};
use pstore_engine::Head;
use pstore_server::Api;
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const A: u64 = 31;
const SERVED: &str = "served";

async fn send<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: u64,
    uri: &str,
    body: &Value,
) -> Value {
    let method = if uri.ends_with("/documents") {
        "PUT"
    } else {
        "POST"
    };
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
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{uri} -> {v}");
    v
}

async fn put(api: &Arc<Api<MemoryStore>>, tenant: u64, index: &str, rows: u32) {
    let docs: Vec<Value> = (0..rows)
        .map(|i| {
            let x = f64::from(i);
            json!({"id": format!("{index}-{i}"), "vector": [x.sin(), x.cos()],
                   "attributes": {"n": i}})
        })
        .collect();
    send(
        api,
        tenant,
        &format!("/v1/indexes/{index}/documents"),
        &json!({"durability": "durable", "documents": docs}),
    )
    .await;
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    api.fold_due(&pstore_server::FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

/// A store holding `A`'s served index, `siblings` idle indexes of `A`, and `others` other
/// tenants with two indexes each.
async fn seeded(siblings: usize, others: u64) -> MemoryStore {
    let store = MemoryStore::new();
    let writer = Api::new(Accounted::new(store.clone()), LaneId(1)).unwrap();
    put(&writer, A, SERVED, 50).await;
    for s in 0..siblings {
        put(&writer, A, &format!("sib{s}"), 3).await;
    }
    for t in 0..others {
        for i in ["one", "two"] {
            put(&writer, 1000 + t, i, 3).await;
        }
    }
    fold(&writer).await;
    store
}

/// The serving node's store: every key it asks about, by any method, whether or not the
/// object exists. `Accounted::ranges` logs only the reads that succeed through `get`,
/// `get_range` and `get_suffix`, so a 404 probe or a sidecar read would pass unseen.
#[derive(Debug, Clone)]
struct Touched {
    inner: MemoryStore,
    keys: Arc<Mutex<Vec<String>>>,
}

impl Touched {
    fn note(&self, key: &Key) {
        self.keys.lock().unwrap().push(key.as_str().to_owned());
    }
}

#[async_trait::async_trait]
impl BlobStore for Touched {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get_immutable(key, class).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get_suffix_as(key, n, class).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.note(key);
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.note(key);
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.note(key);
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.note(key);
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.note(key);
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        for k in keys {
            self.note(k);
        }
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.note(prefix);
        self.inner.list_unrestricted(prefix).await
    }
}

/// What the serving node did: the keys it touched, the engines it holds, and its cache's bytes.
struct Served {
    keys: Vec<String>,
    engines: usize,
    resident: usize,
}

/// Serves the fixed queries on `A`'s served index, then `also` (a tenant and an index) if
/// given, from a fresh node with a fresh cache.
async fn serve(store: &MemoryStore, also: Option<(u64, &str)>) -> Served {
    let core = Arc::new(CacheCore::memory(64 << 20));
    let touched = Touched {
        inner: store.clone(),
        keys: Arc::default(),
    };
    let node = Api::with_cache(
        Accounted::new(touched.clone()),
        LaneId(2),
        Arc::clone(&core),
    )
    .unwrap();
    let mut asks = vec![(A, SERVED)];
    asks.extend(also);
    for (tenant, index) in asks {
        for q in [
            json!({"vector": [1.0, 0.0], "top_k": 10}),
            json!({"rank_by": ["id", "asc"], "top_k": 20}),
            json!({"vector": [0.0, 1.0], "top_k": 5, "filters": ["n", "Gt", 1]}),
        ] {
            send(&node, tenant, &format!("/v1/indexes/{index}/query"), &q).await;
        }
    }
    let resident = Caching::over(Arc::new(MemoryStore::new()), Some(core)).resident_bytes();
    let keys = touched.keys.lock().unwrap().clone();
    Served {
        keys,
        engines: node.engines().await,
        resident,
    }
}

/// Criteria 1 and 2's predicate, as every way it fails: the node touched a key outside `A`'s
/// HEAD, WAL and served index, held other than one engine, or caches other than what the bare
/// arm's node caches.
fn idle_costs(s: &Served, bare: &Served) -> Vec<String> {
    let mut out = Vec::new();
    if s.engines != 1 {
        out.push(format!("{} engines", s.engines));
    }
    let head = Head::key(TenantId(u128::from(A)));
    let wal = format!("/wal/{A}/");
    let served = format!("/tnt/{A}/idx/{SERVED}/");
    for k in &s.keys {
        if *k == head.as_str() || k.contains(&wal) || k.contains(&served) {
            continue;
        }
        let why = if k.contains(&format!("/tnt/{A}/idx/")) {
            "an idle sibling's"
        } else {
            "a foreign"
        };
        out.push(format!("touched {why} key {k}"));
    }
    if s.resident == 0 {
        out.push("the cache holds nothing, so equality says nothing".into());
    }
    if s.resident != bare.resident {
        out.push(format!(
            "the cache holds {} bytes, {} in the bare arm",
            s.resident, bare.resident
        ));
    }
    out
}

/// Asserts that the control failed the predicate in each of the ways named.
fn fails_each(control: &Served, bare: &Served, clauses: &[&str]) {
    let errs = idle_costs(control, bare);
    for c in clauses {
        assert!(
            errs.iter().any(|e| e.contains(c)),
            "the control did not fail on {c:?}: {errs:?}"
        );
    }
}

#[tokio::test]
async fn an_index_of_a_tenant_the_node_does_not_serve_costs_it_nothing() {
    let bare = serve(&seeded(0, 0).await, None).await;
    assert_eq!(idle_costs(&bare, &bare), Vec::<String>::new());
    let crowded = seeded(0, 100).await;
    let s = serve(&crowded, None).await;
    assert_eq!(idle_costs(&s, &bare), Vec::<String>::new());

    // The control: the same node, asked once about another tenant, must fail every clause.
    let control = serve(&crowded, Some((1000, "one"))).await;
    fails_each(
        &control,
        &bare,
        &["engines", "a foreign key 03e8/tnt/1000/", "the cache holds"],
    );
}

#[tokio::test]
async fn an_idle_index_of_a_served_tenant_costs_its_head_entry_and_nothing_else() {
    let bare_store = seeded(0, 0).await;
    let bare = serve(&bare_store, None).await;
    let siblings = seeded(200, 0).await;
    let s = serve(&siblings, None).await;
    assert_eq!(idle_costs(&s, &bare), Vec::<String>::new());

    // The control: one query on one sibling must fail the read clause and the cache clause.
    let control = serve(&siblings, Some((A, "sib7"))).await;
    fails_each(
        &control,
        &bare,
        &["an idle sibling's key", "/idx/sib7/", "the cache holds"],
    );

    // Criterion 3: what it does cost, which is not zero. The engine holds this HEAD decoded.
    let key = Head::key(TenantId(u128::from(A)));
    let with = siblings.get(&key).await.unwrap().len();
    let without = bare_store.get(&key).await.unwrap().len();
    let per = (with - without) / 200;
    println!(
        "HEAD: {without} bytes bare, {with} with 200 idle siblings, {per} bytes per idle index"
    );
    assert!(per > 0, "an idle sibling is reported as free");
}
