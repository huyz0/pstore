//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M54: one index searched by several servers at once, through the API and over real sockets.
//!
//! Three servers share one store, each with the other two as peers; a fourth shares it with
//! none, and is what a split answer must equal. Each server's view of the store records the
//! keys it reads, can fail every read, and can wait before each request.

use crate::peer_fixture;
use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition,
    PutOutcome,
};
use pstore_server::{Api, PeerConfig};
use pstore_types::{CasTag, LaneId};
use serde_json::{Value, json};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

fn peer_addresses() -> &'static Mutex<std::collections::HashMap<usize, String>> {
    static ADDRESSES: std::sync::OnceLock<Mutex<std::collections::HashMap<usize, String>>> =
        std::sync::OnceLock::new();
    ADDRESSES.get_or_init(Mutex::default)
}

/// The wait injected before every read when a test bounds a query's rounds by the clock: long
/// enough that the query's own work fits in the margin below a fifth round. ⚠️ Not longer: at
/// 1 s a peer's two rounds outlast the peer timeout and the coordinator runs the share itself,
/// six rounds in all. Measured locally, four rounds took four waits plus 20 ms at 100 to 500 ms.
const ROUND_MS: u64 = 500;
const ROUND: Duration = Duration::from_millis(ROUND_MS);

/// One server's view of the shared store: what it read, and whether it fails or waits.
#[derive(Debug, Clone)]
struct View {
    inner: MemoryStore,
    read: Arc<Mutex<Vec<String>>>,
    failing: Arc<AtomicBool>,
    delay_ms: Arc<AtomicU64>,
}

impl View {
    fn over(inner: &MemoryStore) -> Self {
        Self {
            inner: inner.clone(),
            read: Arc::default(),
            failing: Arc::default(),
            delay_ms: Arc::default(),
        }
    }
    async fn saw(&self, key: &Key) -> Result<(), BlobError> {
        let ms = self.delay_ms.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        self.read.lock().unwrap().push(key.as_str().to_owned());
        if self.failing.load(Ordering::SeqCst) {
            return Err(BlobError::Other("this server's store is failing".into()));
        }
        Ok(())
    }
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.read.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl BlobStore for View {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.saw(key).await?;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.saw(key).await?;
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        _: Class,
    ) -> Result<Bytes, BlobError> {
        self.saw(key).await?;
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.saw(key).await?;
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        _: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.saw(key).await?;
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.saw(key).await?;
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, _: Class) -> Result<Bytes, BlobError> {
        self.saw(key).await?;
        self.inner.get_suffix(key, n).await
    }
    async fn get_immutable(&self, key: &Key, _: Class) -> Result<Bytes, BlobError> {
        self.saw(key).await?;
        self.inner.get(key).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.saw(key).await?;
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.saw(key).await?;
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.saw(key).await?;
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
}

/// A server: its API, its view of the store, and its URL.
struct Server {
    api: Arc<Api<View>>,
    view: View,
    url: String,
}

fn params() -> pstore_index::cluster::Params {
    pstore_index::cluster::Params {
        // Every segment clustered, so a dense leg reads its centroid table: the mark of where
        // it ran.
        exact_scan_threshold: 8,
        ..pstore_index::cluster::Params::default()
    }
}

/// A peer's TCP listener and the UDP socket HTTP/3 shares its port with.
type Sockets = (tokio::net::TcpListener, std::net::UdpSocket);

/// Both peer sockets on one free port. ⚠️ Binding UDP to whatever port TCP was given raced
/// `udp_port`: a gossip socket another test held on that number refused the bind with
/// `AddrInUse`, so a port whose UDP half is taken is skipped rather than unwrapped.
async fn sockets() -> Sockets {
    for _ in 0..64 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        if let Ok(udp) = std::net::UdpSocket::bind(listener.local_addr().unwrap()) {
            return (listener, udp);
        }
    }
    panic!("no port free for both TCP and UDP in 64 tries");
}

/// A server over `store`, listening on a port of its own.
async fn start(store: &MemoryStore, lane: u64) -> (Server, Sockets) {
    let view = View::over(store);
    let api = Api::with_auth(
        Accounted::new(view.clone()),
        LaneId(lane),
        peer_fixture::auth(),
    )
    .unwrap();
    api.set_index_params_for_test(params());
    let (listener, udp) = sockets().await;
    let url = format!("https://{}", listener.local_addr().unwrap());
    peer_addresses()
        .lock()
        .unwrap()
        .insert(Arc::as_ptr(&api) as usize, url.clone());
    (Server { api, view, url }, (listener, udp))
}

fn serve(server: &Server, (listener, udp): Sockets) {
    tokio::spawn(pstore_server::serve_peers(
        Arc::clone(&server.api),
        listener,
        udp,
        std::future::pending(),
    ));
}

/// Three peered servers and one alone, over one store; `extra` are more URLs in the peer list.
async fn cluster(extra: &[String]) -> (MemoryStore, Vec<Server>, Server) {
    let store = MemoryStore::new();
    let mut servers = Vec::new();
    let mut listeners = Vec::new();
    for lane in 1..=3 {
        let (s, l) = start(&store, lane).await;
        servers.push(s);
        listeners.push(l);
    }
    let mut urls: Vec<String> = servers.iter().map(|s| s.url.clone()).collect();
    urls.extend(extra.iter().cloned());
    for (me, s) in servers.iter().enumerate() {
        s.api
            .set_peers(Some(PeerConfig {
                servers: urls.clone(),
                me,
                timeout: Duration::from_secs(5),
            }))
            .unwrap();
    }
    for (s, l) in servers.iter().zip(listeners) {
        serve(s, l);
    }
    let (alone, _) = start(&store, 9).await;
    (store, servers, alone)
}

async fn send(api: &Arc<Api<View>>, method: &str, uri: &str, body: Value) -> (u16, Value) {
    if uri == "/peer/v1/part" {
        assert_eq!(method, "POST");
        let address = peer_addresses()
            .lock()
            .unwrap()
            .get(&(Arc::as_ptr(api) as usize))
            .unwrap()
            .clone();
        return peer_fixture::call(&address, "7", body).await;
    }
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

async fn query(api: &Arc<Api<View>>, q: &Value) -> (u16, Value) {
    send(api, "POST", "/v1/indexes/docs/query", q.clone()).await
}

async fn metric(api: &Arc<Api<View>>, name: &str) -> u64 {
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{name} ")))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{name} not in /metrics"))
}

fn doc(i: usize) -> Value {
    let x = i as f32;
    json!({
        "id": format!("d{i:04}"),
        "vector": [x.sin(), x.cos(), (x * 0.37).sin(), 1.0],
        "text": format!("common word{} tag{}", i % 5, i % 3),
        "attributes": {"n": i},
    })
}

/// The segment keys HEAD names now: every `.seg` object under the store.
async fn segments(store: &MemoryStore) -> Vec<String> {
    store
        .list_unrestricted(&Key::new(String::new()))
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.as_str().to_owned())
        .filter(|k| k.ends_with(".seg"))
        .collect()
}

/// Folds segments of 24 rows through `api` until there are at least `min` and every one of
/// `urls` is assigned at least one; then deletes some rows and folds once more.
async fn fill(store: &MemoryStore, api: &Arc<Api<View>>, urls: &[String], min: usize) {
    fill_with(store, api, urls, min, doc).await;
}

/// [`fill`], with documents `make` writes.
async fn fill_with(
    store: &MemoryStore,
    api: &Arc<Api<View>>,
    urls: &[String],
    min: usize,
    make: fn(usize) -> Value,
) {
    fill_declared(store, api, urls, min, make, &json!({})).await;
}

/// [`fill_with`], every write declaring `schema` (M154).
async fn fill_declared(
    store: &MemoryStore,
    api: &Arc<Api<View>>,
    urls: &[String],
    min: usize,
    make: fn(usize) -> Value,
    schema: &Value,
) {
    let mut k = 0;
    loop {
        let docs: Vec<Value> = (k * 24..k * 24 + 24).map(make).collect();
        let mut body = json!({"durability": "durable", "documents": docs});
        if schema.as_object().is_some_and(|o| !o.is_empty()) {
            body["schema"] = schema.clone();
        }
        let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", body).await;
        assert_eq!(s, 200, "{b}");
        let (s, b) = send(api, "POST", "/v1/admin/fold", Value::Null).await;
        assert_eq!(s, 200, "{b}");
        k += 1;
        let segs = segments(store).await;
        let mut held = vec![0; urls.len()];
        for seg in &segs {
            held[pstore_engine::assign(seg, urls)] += 1;
        }
        if segs.len() >= min && held.iter().all(|n| *n > 0) {
            break;
        }
        assert!(k < 40, "no fixture gives every server a segment: {held:?}");
    }
    let deletes: Vec<String> = (0..k * 24).step_by(9).map(|i| format!("d{i:04}")).collect();
    let (s, b) = send(
        api,
        "PUT",
        "/v1/indexes/docs/documents",
        json!({"durability": "durable", "deletes": deletes}),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    send(api, "POST", "/v1/admin/fold", Value::Null).await;
}

/// What a split answer must equal: the results, floats as written, and the epoch.
fn answer(v: &Value) -> Value {
    json!({"results": v["results"], "epoch": v["meta"]["epoch"]})
}

fn urls(servers: &[Server]) -> Vec<String> {
    servers.iter().map(|s| s.url.clone()).collect()
}

const Q: [f32; 4] = [0.3, 0.9, -0.2, 1.0];

fn queries() -> Vec<(&'static str, Value)> {
    let filter = json!(["n", "Gt", 40]);
    vec![
        (
            "dense",
            json!({"vector": Q, "top_k": 10, "include_attributes": true}),
        ),
        (
            "dense, filtered",
            json!({"vector": Q, "top_k": 10, "filters": filter}),
        ),
        (
            "dense, exact",
            json!({"vector": Q, "top_k": 10, "exact": true}),
        ),
        (
            "hybrid, rrf",
            json!({"vector": Q, "text": "word2 tag1", "top_k": 10}),
        ),
        (
            "hybrid, weighted, filtered",
            json!({"vector": Q, "text": "word2 tag1", "top_k": 10, "filters": filter,
                   "fusion": {"rrf": {"weights": [2.0, 0.5]}}}),
        ),
        (
            "multi-query",
            json!({"queries": [{"vector": Q, "top_k": 5},
                               {"vector": Q, "text": "word3", "top_k": 5}]}),
        ),
        // M55: text legs split too.
        ("text", json!({"text": "word2 tag1", "top_k": 10})),
        // M58: `sum` splits too, cut by sum.
        (
            "sum, weighted",
            json!({"text": ["word2 tag1", "common word3"], "top_k": 10,
                   "fusion": {"sum": {"weights": [2.0, 0.5]}}}),
        ),
        (
            "text, filtered",
            json!({"text": "word3 common", "top_k": 10, "filters": filter}),
        ),
    ]
}

/// Exchanges `api` has answered on the part endpoint: one per part in one exchange, two per
/// phased part (M55).
async fn exchanges(api: &Arc<Api<View>>) -> u64 {
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    text.lines()
        .filter(|line| line.starts_with("pstore_peer_parts_completed_total{transport=\"h2\","))
        .map(|line| line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
        .sum()
}

#[tokio::test]
async fn a_split_query_answers_exactly_as_one_server_does() {
    let (store, servers, alone) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    for (name, q) in queries() {
        let (s0, want) = query(&alone.api, &q).await;
        assert_eq!(s0, 200, "{name}: {want}");
        for (i, server) in servers.iter().enumerate() {
            let sent = metric(&server.api, "pstore_peer_parts_sent").await;
            let (s, got) = query(&server.api, &q).await;
            assert_eq!(s, 200, "{name} at {i}: {got}");
            if name == "multi-query" {
                assert_eq!(got["results"], want["results"], "{name} at {i}");
            } else {
                assert!(!want["results"].as_array().unwrap().is_empty());
                assert_eq!(answer(&got), answer(&want), "{name} at {i}");
            }
            assert!(
                metric(&server.api, "pstore_peer_parts_sent").await > sent,
                "{name} at {i} was not split"
            );
        }
    }
    for s in &servers {
        assert_eq!(metric(&s.api, "pstore_peer_parts_failed").await, 0);
    }
}

#[tokio::test]
async fn each_segment_is_scanned_once_by_its_server() {
    let (store, servers, _) = cluster(&[]).await;
    let all = urls(&servers);
    fill(&store, &servers[0].api, &all, 8).await;
    let segs = segments(&store).await;
    // Coordinated from each server in turn: a server that mistook which one it is would scan
    // another's segments itself, and send its own away (the sweep found `me` unchecked when
    // only the first server coordinated).
    for (c, coordinator) in servers.iter().enumerate() {
        let served: Vec<u64> = futures_util::future::join_all(
            servers
                .iter()
                .map(|s| metric(&s.api, "pstore_peer_parts_served")),
        )
        .await;
        let sent = metric(&coordinator.api, "pstore_peer_parts_sent").await;
        // No read cache on any server, so every table a leg needs is read, by the server
        // whose leg it is.
        for s in &servers {
            s.view.take();
        }
        let (status, body) = query(&coordinator.api, &json!({"vector": Q, "top_k": 10})).await;
        assert_eq!(status, 200, "{body}");
        let read: Vec<Vec<String>> = servers.iter().map(|s| s.view.take()).collect();
        let live: std::collections::BTreeSet<String> = read[c]
            .iter()
            .filter(|k| k.ends_with(".seg"))
            .cloned()
            .collect();
        assert!(
            live.len() >= 8,
            "coordinator {c} opened {} segments",
            live.len()
        );
        let mut each = [0usize; 3];
        for seg in segs.iter().filter(|s| live.contains(*s)) {
            let cen = format!("{seg}.cen");
            let owner = pstore_engine::assign(seg, &all);
            for (i, r) in read.iter().enumerate() {
                let n = r.iter().filter(|k| **k == cen).count();
                if i == owner {
                    assert_eq!(n, 1, "coordinator {c}: {seg}'s table read {n} times by {i}");
                    each[i] += 1;
                } else {
                    assert_eq!(
                        n, 0,
                        "coordinator {c}: {seg}'s table read by {i}, not {owner}"
                    );
                }
            }
            // The coordinator reads every footer: its row fetch needs them.
            assert!(read[c].iter().any(|k| k == seg), "{seg}'s footer");
        }
        assert!(
            each.iter().all(|n| *n > 0),
            "a server scanned nothing: {each:?}"
        );
        assert_eq!(
            metric(&coordinator.api, "pstore_peer_parts_sent").await,
            sent + 2
        );
        for (i, s) in servers.iter().enumerate().filter(|(i, _)| *i != c) {
            assert_eq!(
                metric(&s.api, "pstore_peer_parts_served").await,
                served[i] + 1,
                "coordinator {c}, server {i}"
            );
        }
    }
}

#[tokio::test]
async fn a_split_query_keeps_the_round_trip_budget() {
    // ⚠️ Wall clock under an injected wait per request: four rounds take four waits, and a
    // chain of five -- the peers called only after the coordinator's own open -- at least five,
    // because a sleep never ends early. A per-store round counter cannot tell the two apart:
    // each store sees its own rounds, not the chain. So the bound is just under five waits,
    // and the wait is long enough that the rest of the query fits in the margin: at 250 ms and
    // a bound of 4.5 waits, macOS CI took 1.2 s with every server test in one binary.
    let (store, servers, _) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let q = json!({"vector": Q, "top_k": 10});
    // Warm nothing; but make sure the query itself is sound first.
    let sent = metric(&servers[0].api, "pstore_peer_parts_sent").await;
    for s in &servers {
        s.view.delay_ms.store(ROUND_MS, Ordering::SeqCst);
    }
    let started = std::time::Instant::now();
    let (status, body) = query(&servers[0].api, &q).await;
    let took = started.elapsed();
    for s in &servers {
        s.view.delay_ms.store(0, Ordering::SeqCst);
    }
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        metric(&servers[0].api, "pstore_peer_parts_sent").await,
        sent + 2
    );
    assert!(
        took < 5 * ROUND,
        "a split query took {took:?}: five rounds of {ROUND:?} or more"
    );
}

/// M165: explain counts the segments a peer answered as remote, and a share that failed and
/// ran here as local.
#[tokio::test]
async fn explain_counts_remote_segments() {
    let (store, servers, _) = cluster(&[]).await;
    let all = urls(&servers);
    fill(&store, &servers[0].api, &all, 8).await;
    let segs = segments(&store).await;
    let remote = segs
        .iter()
        .filter(|s| pstore_engine::assign(s, &all) != 0)
        .count();
    assert!(
        remote > 0 && remote < segs.len(),
        "{remote} of {}",
        segs.len()
    );
    let q = json!({"vector": Q, "top_k": 10, "explain": true});
    let local = |x: &Value| {
        x["legs"][0]["approximate_segments"].as_u64().unwrap()
            + x["legs"][0]["exact_segments"].as_u64().unwrap()
    };
    let (s, got) = query(&servers[0].api, &q).await;
    assert_eq!(s, 200, "{got}");
    let x = &got["meta"]["explain"];
    assert_eq!(x["segments"], json!(segs.len()), "{x}");
    assert_eq!(x["remote_segments"], json!(remote), "{x}");
    assert_eq!(local(x), (segs.len() - remote) as u64, "{x}");
    // A text leg's shares are phased (M55): their segments are remote too.
    let t = json!({"text": "word2 tag1", "top_k": 10, "explain": true});
    let (s, got) = query(&servers[0].api, &t).await;
    assert_eq!(s, 200, "{got}");
    let x = &got["meta"]["explain"];
    assert_eq!(x["segments"], json!(segs.len()), "{x}");
    assert_eq!(x["remote_segments"], json!(remote), "{x}");
    // Both peers failing: every share runs here.
    for s in &servers[1..] {
        s.view.failing.store(true, Ordering::SeqCst);
    }
    let (s, got) = query(&servers[0].api, &q).await;
    assert_eq!(s, 200, "{got}");
    let x = &got["meta"]["explain"];
    assert_eq!(x["remote_segments"], json!(0), "{x}");
    assert_eq!(local(x), segs.len() as u64, "{x}");
}

#[tokio::test]
async fn a_failed_peer_costs_rounds_not_answers() {
    // A server in the list that is not listening: its segments' parts fail to connect.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("https://{}", l.local_addr().unwrap())
    };
    let (store, servers, alone) = cluster(std::slice::from_ref(&dead)).await;
    let mut all = urls(&servers);
    all.push(dead);
    fill(&store, &servers[0].api, &all, 8).await;
    // Another peer's store fails every read.
    servers[1].view.failing.store(true, Ordering::SeqCst);
    for (name, q) in queries() {
        let (_, want) = query(&alone.api, &q).await;
        let before = metric(&servers[0].api, "pstore_peer_parts_failed").await;
        let (s, got) = query(&servers[0].api, &q).await;
        assert_eq!(s, 200, "{name}: {got}");
        assert_eq!(got["results"], want["results"], "{name}");
        // Both failed parts of each sub-query, each counted once.
        let parts = if name == "multi-query" { 4 } else { 2 };
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_failed").await,
            before + parts,
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_failed_peer_costs_rounds_not_answers_for_a_multi_leg() {
    // M94: the share of a `multi` or `multi_fde` leg that fails is run here, alone, beside a
    // dense leg, and beside a text leg (two phases).
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("https://{}", l.local_addr().unwrap())
    };
    let (store, servers, alone) = cluster(std::slice::from_ref(&dead)).await;
    let mut all = urls(&servers);
    all.push(dead);
    let late = |i: usize| {
        let mut d = doc(i);
        let x = i as f32;
        d["vectors"] = json!({"late": [[x.sin(), 1.0], [x.cos(), -1.0]]});
        d
    };
    fill_with(&store, &servers[0].api, &all, 8, late).await;
    servers[1].view.failing.store(true, Ordering::SeqCst);
    let multi = json!({"field": "late", "vectors": [[0.5, 0.25]]});
    let fde = json!({"field": "late", "vectors": [[0.5, 0.25]], "mode": "fde"});
    for q in [
        json!({"multi": multi, "top_k": 10}),
        json!({"multi": fde, "top_k": 10}),
        json!({"multi": multi, "vector": Q, "top_k": 10}),
        json!({"multi": fde, "text": "word2", "top_k": 10}),
    ] {
        let (_, want) = query(&alone.api, &q).await;
        let before = metric(&servers[0].api, "pstore_peer_parts_failed").await;
        let (s, got) = query(&servers[0].api, &q).await;
        assert_eq!(s, 200, "{q}: {got}");
        assert_eq!(got["results"], want["results"], "{q}");
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_failed").await,
            before + 2,
            "{q}"
        );
    }
}

#[tokio::test]
async fn a_peer_of_another_protocol_is_refused_and_run_here() {
    // A peer that answers every part with the version refusal.
    let fake = axum::Router::new().route(
        "/peer/v1/part",
        axum::routing::post(|| async { (axum::http::StatusCode::CONFLICT, "protocol_mismatch") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(peer_fixture::serve_fake(listener, fake));
    let (store, servers, alone) = cluster(std::slice::from_ref(&other)).await;
    let mut all = urls(&servers);
    all.push(other);
    fill(&store, &servers[0].api, &all, 8).await;
    let q = json!({"vector": Q, "top_k": 10});
    let (_, want) = query(&alone.api, &q).await;
    let before = metric(&servers[0].api, "pstore_peer_parts_failed").await;
    let (s, got) = query(&servers[0].api, &q).await;
    assert_eq!(s, 200, "{got}");
    assert_eq!(answer(&got), answer(&want));
    assert_eq!(
        metric(&servers[0].api, "pstore_peer_parts_failed").await,
        before + 1
    );
    // And a real server refuses a part of another version with that status.
    let (status, body) = send(
        &servers[1].api,
        "POST",
        "/peer/v1/part",
        json!({"protocol": 999, "index": "docs", "fts": "", "filters": null, "shadow": 0,
               "legs": [], "targets": []}),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    // A phase is protocol 2's alone (sweep): protocol 1 naming one is refused too.
    for phase in ["open", "scan"] {
        let (status, body) = send(
            &servers[1].api,
            "POST",
            "/peer/v1/part",
            json!({"protocol": 1, "phase": phase, "index": "docs", "fts": "", "filters": null,
                   "shadow": 0, "legs": [], "targets": [], "id": "00", "stats": {"doc_count":0,"total_tokens":0,"df":[]}}),
        )
        .await;
        assert_eq!(status, 409, "{phase}: {body}");
    }
}

#[tokio::test]
async fn a_query_error_on_a_peer_is_the_clients() {
    let (store, servers, alone) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    // In one exchange, and in two (M55: the sweep found a phased `422` uncounted only by
    // accident of the `&&` that counts it).
    for q in [
        json!({"vector": [1.0, 2.0], "top_k": 10}),
        json!({"vector": [1.0, 2.0], "text": "word2", "top_k": 10}),
    ] {
        let (s0, want) = query(&alone.api, &q).await;
        let sent = metric(&servers[0].api, "pstore_peer_parts_sent").await;
        let (s, got) = query(&servers[0].api, &q).await;
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_sent").await,
            sent + 2,
            "the wrong-dimension query must reach the peers to test their error: {q}"
        );
        assert_eq!((s, &got["error"]), (s0, &want["error"]), "{got} / {want}");
        assert!((400..500).contains(&s), "{s} {got}");
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_failed").await,
            0,
            "{q}"
        );
    }
}

#[tokio::test]
async fn a_part_outside_its_tenant_is_refused() {
    let (_, servers, _) = cluster(&[]).await;
    let ours = "0007/tnt/7/idx/docs/seg/L0/00000000000000000001-0000000000000001.seg";
    let part = |segment: &str, deleted: Option<&str>, legs: usize| {
        let leg = json!({"kind": "sparse", "j": 0, "field": "s", "query": [], "limit": 1});
        json!({"protocol": 1, "index": "docs",
               "fts": pstore_format::text::FullText::default().encode(),
               "filters": null, "shadow": 0,
               "legs": vec![leg; legs],
               "targets": [{"i": 0, "segment": segment, "segment_len": null,
                            "centroids": false, "deleted": deleted, "sparse_dict": true,
                            "text_dict": false, "shadowed": false}]})
    };
    let refused = [
        part("0008/tnt/8/idx/docs/seg/L0/1.seg", None, 1),
        part(
            "0007/tnt/7/idx/docs/seg/../../../../0008/tnt/8/x.seg",
            None,
            1,
        ),
        part("0007/tnt/7/idx/../idx/docs/seg/L0/1.seg", None, 1),
        part("0007/tnt/7/idx/docs/seg/L0/1.cen", None, 1),
        part("0007/tnt/7/lanes/1.seg", None, 1),
        part(ours, Some("0008/tnt/8/idx/docs/seg/L0/1.seg.1-1.dv"), 1),
        part(ours, Some(&format!("{ours}.1-1.seg")), 1),
        // Each condition alone: another segment's vector, and a `..` under this one.
        part(
            ours,
            Some("0007/tnt/7/idx/docs/seg/L0/00000000000000000002-0000000000000001.seg.1-1.dv"),
            1,
        ),
        part(ours, Some(&format!("{ours}/../../../../../0008/x.dv")), 1),
        part(ours, None, pstore_query::MAX_LEGS + 1),
    ];
    let s = &servers[1];
    for p in refused {
        s.view.take();
        let (status, body) = send(&s.api, "POST", "/peer/v1/part", p.clone()).await;
        assert_eq!(status, 400, "{p} -> {body}");
        assert!(
            s.view.take().is_empty(),
            "a refused part read the store: {p}"
        );
    }
    // An index named `a..b` is one of the tenant's: `..` refuses only as a path component.
    let dotted = "0007/tnt/7/idx/a..b/seg/L0/00000000000000000001-0000000000000001.seg";
    let (status, body) = send(&s.api, "POST", "/peer/v1/part", part(dotted, None, 1)).await;
    assert_ne!(status, 400, "{body}");
    // The same part with its own keys is admitted, and reads.
    let (status, body) = send(
        &s.api,
        "POST",
        "/peer/v1/part",
        part(ours, Some(&format!("{ours}.1-1.dv")), 1),
    )
    .await;
    assert_ne!(status, 400, "{body}");
}

#[tokio::test]
async fn queries_that_cannot_be_split_run_on_one_server() {
    let (store, servers, _) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let a = &servers[0].api;
    let (_, past) = query(a, &json!({"vector": Q, "top_k": 1})).await;
    let epoch = past["meta"]["epoch"].clone();
    // M55 narrowed this list: a text-only query splits now; M58: `sum` too (their own
    // assertions below).
    let unsplit = [
        json!({"rank_by": ["n", "asc"], "top_k": 5}),
        json!({"aggregate_by": {"c": ["Count", "id"]}}),
        json!({"vector": Q, "top_k": 5, "as_of": epoch}),
    ];
    for (q, name) in [
        (json!({"text": "word2", "top_k": 5}), "text-only, M55"),
        (
            json!({"text": "word2", "top_k": 5, "fusion": {"sum": {}}}),
            "sum, M58",
        ),
    ] {
        let sent = metric(a, "pstore_peer_parts_sent").await;
        let (s, body) = query(a, &q).await;
        assert_eq!(s, 200, "{body}");
        assert!(metric(a, "pstore_peer_parts_sent").await > sent, "{name}");
    }
    for q in &unsplit {
        let sent = metric(a, "pstore_peer_parts_sent").await;
        let (s, body) = query(a, q).await;
        assert_eq!(s, 200, "{q}: {body}");
        assert_eq!(
            metric(a, "pstore_peer_parts_sent").await,
            sent,
            "{q} was split"
        );
    }
    // An index of one segment.
    send(
        a,
        "PUT",
        "/v1/indexes/one/documents",
        json!({"durability": "durable", "documents": [doc(1), doc(2)]}),
    )
    .await;
    send(a, "POST", "/v1/admin/fold", Value::Null).await;
    let sent = metric(a, "pstore_peer_parts_sent").await;
    let (s, body) = send(
        a,
        "POST",
        "/v1/indexes/one/query",
        json!({"vector": Q, "top_k": 2}),
    )
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(metric(a, "pstore_peer_parts_sent").await, sent);
}

/// M180: a query with a prefix leg is answered by its coordinator alone, as one server
/// answers it, and sends no part -- the peer protocol does not carry the option.
#[tokio::test]
async fn a_prefix_query_is_not_split() {
    let (store, servers, alone) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let q = json!({"text": [{"field": "text", "query": "common wor", "last_as_prefix": true}],
                   "vector": Q, "top_k": 10});
    let (s0, want) = query(&alone.api, &q).await;
    assert_eq!(s0, 200, "{want}");
    assert!(!want["results"].as_array().unwrap().is_empty());
    for (i, server) in servers.iter().enumerate() {
        let sent = metric(&server.api, "pstore_peer_parts_sent").await;
        let (s, got) = query(&server.api, &q).await;
        assert_eq!(s, 200, "at {i}: {got}");
        assert_eq!(answer(&got), answer(&want), "at {i}");
        assert_eq!(
            metric(&server.api, "pstore_peer_parts_sent").await,
            sent,
            "at {i}: split"
        );
    }
}

#[tokio::test]
async fn a_split_boosted_query_answers_as_one_server_does() {
    // M62: the legs and the engine's `top_k` are the window, so a split query's candidates are
    // the unpeered server's, and so is their rescoring.
    let (store, servers, alone) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let boost = json!({"window": 40, "terms": [
        {"attr": "n", "fn": "decay", "weight": 0.02, "origin": 100, "scale": 30},
        {"attr": "n", "fn": "linear", "weight": 0.0001}]});
    for q in [
        json!({"vector": Q, "top_k": 10, "boost": boost}),
        json!({"text": "word2 tag1", "top_k": 10, "boost": boost}),
        json!({"vector": Q, "text": "word2", "top_k": 10, "boost": boost}),
    ] {
        let (s0, want) = query(&alone.api, &q).await;
        assert_eq!(s0, 200, "{q}: {want}");
        assert_eq!(want["results"].as_array().map(Vec::len), Some(10), "{want}");
        for (i, server) in servers.iter().enumerate() {
            let sent = metric(&server.api, "pstore_peer_parts_sent").await;
            let (s, got) = query(&server.api, &q).await;
            assert_eq!(s, 200, "{q} at {i}: {got}");
            assert_eq!(answer(&got), answer(&want), "{q} at {i}");
            assert!(
                metric(&server.api, "pstore_peer_parts_sent").await > sent,
                "{q} at {i} was not split"
            );
        }
    }
}

#[tokio::test]
async fn a_multi_query_is_split() {
    // M94 (row 64): a `multi` leg crosses the part protocol, alone or beside a leg that
    // splits, and the answer is the one server's.
    let (store, servers, alone) = cluster(&[]).await;
    let late = |i: usize| {
        let mut d = doc(i);
        let x = i as f32;
        d["vectors"] = json!({"late": [[x.sin(), 1.0], [x.cos(), -1.0]]});
        d
    };
    fill_with(&store, &servers[0].api, &urls(&servers), 8, late).await;
    let multi = json!({"field": "late", "vectors": [[0.5, 0.25]]});
    let fde = json!({"field": "late", "vectors": [[0.5, 0.25]], "mode": "fde"});
    for q in [
        json!({"multi": multi, "top_k": 10}),
        json!({"multi": multi, "text": "word2", "top_k": 10}),
        json!({"multi": multi, "vector": Q, "top_k": 10}),
        // M67: the encoding's leg splits too.
        json!({"multi": fde, "top_k": 10}),
        json!({"multi": fde, "text": "word2", "top_k": 10}),
        json!({"multi": fde, "vector": Q, "top_k": 10}),
    ] {
        let (s0, want) = query(&alone.api, &q).await;
        assert_eq!(s0, 200, "{q}: {want}");
        assert_eq!(want["results"].as_array().map(Vec::len), Some(10), "{want}");
        for (i, server) in servers.iter().enumerate() {
            let sent = metric(&server.api, "pstore_peer_parts_sent").await;
            let (s, got) = query(&server.api, &q).await;
            assert_eq!(s, 200, "{q} at {i}: {got}");
            assert_eq!(answer(&got), answer(&want), "{q} at {i}");
            assert!(
                metric(&server.api, "pstore_peer_parts_sent").await > sent,
                "{q} at {i} was not split"
            );
        }
    }
}

#[test]
fn peers_are_both_or_neither_and_include_self() {
    let read = |vars: &[(&str, &str)]| {
        let vars: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        PeerConfig::from_vars(|k| vars.get(k).cloned())
    };
    assert_eq!(read(&[]).unwrap(), None);
    let ok = read(&[
        ("PSTORE_PEERS", "https://a:1, https://b:1/,https://c:1"),
        ("PSTORE_PEER_SELF", "https://b:1/"),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(ok.servers, ["https://a:1", "https://b:1", "https://c:1"]);
    assert_eq!(ok.me, 1);
    assert_eq!(ok.timeout, Duration::from_millis(2_000));
    let timed = read(&[
        ("PSTORE_PEERS", "https://a:1,https://b:1"),
        ("PSTORE_PEER_SELF", "https://a:1"),
        ("PSTORE_PEER_TIMEOUT_MS", "150"),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(timed.timeout, Duration::from_millis(150));
    for bad in [
        vec![("PSTORE_PEERS", "https://a:1,https://b:1")],
        vec![("PSTORE_PEER_SELF", "https://a:1")],
        vec![
            ("PSTORE_PEERS", "https://a:1,https://b:1"),
            ("PSTORE_PEER_SELF", "https://c:1"),
        ],
        vec![
            ("PSTORE_PEERS", "https://a:1,https://a:1/"),
            ("PSTORE_PEER_SELF", "https://a:1"),
        ],
        vec![
            ("PSTORE_PEERS", "https://a:1"),
            ("PSTORE_PEER_SELF", "https://a:1"),
            ("PSTORE_PEER_TIMEOUT_MS", "0"),
        ],
        vec![("PSTORE_PEER_TIMEOUT_MS", "soon")],
        vec![
            ("PSTORE_PEERS", "http://a:1,https://b:1"),
            ("PSTORE_PEER_SELF", "https://b:1"),
        ],
        vec![
            ("PSTORE_PEERS", "https://a:1/v1,https://b:1"),
            ("PSTORE_PEER_SELF", "https://b:1"),
        ],
        vec![
            ("PSTORE_PEERS", "a:1,https://b:1"),
            ("PSTORE_PEER_SELF", "https://b:1"),
        ],
    ] {
        assert!(read(&bad).is_err(), "{bad:?}");
    }
}

#[tokio::test]
async fn failed_shares_are_run_here_together() {
    // Code review: two failed shares run one after the other cost four more rounds. Run
    // together, two: HEAD, open, legs, the failed shares' open and legs, rows -- six rounds of
    // 400 ms (2.4 s), where one after the other is eight (3.2 s).
    //
    // M116: the dead peer accepts and closes, never a closed port. Windows retries a connect
    // the port refused before reporting it, which costs this timed query a second or more;
    // a connection closed at once fails in one loopback round trip on every OS.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for c in l.incoming() {
                drop(c);
            }
        });
        url
    };
    let (store, servers, _) = cluster(std::slice::from_ref(&dead)).await;
    let mut all = urls(&servers);
    all.push(dead);
    fill(&store, &servers[0].api, &all, 8).await;
    servers[1].view.failing.store(true, Ordering::SeqCst);
    for s in &servers {
        s.view.delay_ms.store(400, Ordering::SeqCst);
    }
    let before = metric(&servers[0].api, "pstore_peer_parts_failed").await;
    let started = std::time::Instant::now();
    let (status, body) = query(&servers[0].api, &json!({"vector": Q, "top_k": 10})).await;
    let took = started.elapsed();
    for s in &servers {
        s.view.delay_ms.store(0, Ordering::SeqCst);
    }
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        metric(&servers[0].api, "pstore_peer_parts_failed").await,
        before + 2
    );
    assert!(
        took < Duration::from_millis(2_900),
        "two failed shares took {took:?}: more than six rounds of 400 ms"
    );
}

#[tokio::test]
async fn a_split_filtered_query_masks_each_segment_on_its_server() {
    // The sweep found the coordinator's masks unchecked: computing a mask for a segment whose
    // vector legs run elsewhere changes no answer, only reads every block it admits -- the
    // reads the split exists to spread. So the coordinator reads a segment that holds no
    // answer row once: its footer.
    let (store, servers, _) = cluster(&[]).await;
    let all = urls(&servers);
    fill(&store, &servers[0].api, &all, 8).await;
    let segs = segments(&store).await;
    for s in &servers {
        s.view.take();
    }
    let (status, body) = query(
        &servers[0].api,
        &json!({"vector": Q, "top_k": 1, "filters": ["n", "Gt", 40]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let read = servers[0].view.take();
    let live: Vec<&String> = segs.iter().filter(|s| read.contains(s)).collect();
    let remote: Vec<&&String> = live
        .iter()
        .filter(|s| pstore_engine::assign(s, &all) != 0)
        .collect();
    let once = remote
        .iter()
        .filter(|s| read.iter().filter(|k| k == **s).count() == 1)
        .count();
    // One remote segment at most holds the one answer row, and is read once more for it.
    assert!(
        once + 1 >= remote.len() && !remote.is_empty(),
        "remote segments read more than their footer: {} of {}",
        remote.len() - once,
        remote.len()
    );
}

// ---- M55: text legs split too ------------------------------------------------------------

#[tokio::test]
async fn each_text_segment_is_scanned_once_by_its_server() {
    let (store, servers, _) = cluster(&[]).await;
    let all = urls(&servers);
    fill(&store, &servers[0].api, &all, 8).await;
    let segs = segments(&store).await;
    for (c, coordinator) in servers.iter().enumerate() {
        for s in &servers {
            s.view.take();
        }
        let (status, body) = query(
            &coordinator.api,
            &json!({"vector": Q, "text": "word2 tag1", "top_k": 10}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let read: Vec<Vec<String>> = servers.iter().map(|s| s.view.take()).collect();
        let mut each = [0usize; 3];
        for seg in &segs {
            let tdict = format!("{seg}.tdict");
            let owner = pstore_engine::assign(seg, &all);
            for (i, r) in read.iter().enumerate() {
                let n = r.iter().filter(|k| **k == tdict).count();
                if i == owner {
                    assert_eq!(
                        n, 1,
                        "coordinator {c}: {seg}'s dictionary read {n} times by {i}"
                    );
                    each[i] += 1;
                } else {
                    assert_eq!(n, 0, "coordinator {c}: {seg}'s dictionary read by {i}");
                }
            }
        }
        assert!(each.iter().all(|n| *n > 0), "coordinator {c}: {each:?}");
    }
}

#[tokio::test]
async fn a_split_text_query_keeps_the_round_trip_budget() {
    // As M54's, for a hybrid query in two phases: the statistics exchange between them is a
    // round trip between servers, never a blob round. The bound is as M54's, for its reason.
    let (store, servers, _) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let q = json!({"vector": Q, "text": "word2 tag1", "top_k": 10});
    let sent = metric(&servers[0].api, "pstore_peer_parts_sent").await;
    for s in &servers {
        s.view.delay_ms.store(ROUND_MS, Ordering::SeqCst);
    }
    let started = std::time::Instant::now();
    let (status, body) = query(&servers[0].api, &q).await;
    let took = started.elapsed();
    for s in &servers {
        s.view.delay_ms.store(0, Ordering::SeqCst);
    }
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        metric(&servers[0].api, "pstore_peer_parts_sent").await,
        sent + 2
    );
    assert!(
        took < 5 * ROUND,
        "a split text query took {took:?}: five rounds of {ROUND:?} or more"
    );
}

#[tokio::test]
async fn a_vector_query_makes_one_exchange_and_an_old_peer_is_run_here() {
    let (store, servers, alone) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let before: Vec<u64> =
        futures_util::future::join_all(servers.iter().map(|s| exchanges(&s.api))).await;
    let (s, body) = query(&servers[0].api, &json!({"vector": Q, "top_k": 10})).await;
    assert_eq!(s, 200, "{body}");
    for (i, srv) in servers.iter().enumerate().skip(1) {
        assert_eq!(
            exchanges(&srv.api).await,
            before[i] + 1,
            "a vector part, at {i}"
        );
    }
    let (s, body) = query(&servers[0].api, &json!({"text": "word2", "top_k": 10})).await;
    assert_eq!(s, 200, "{body}");
    for (i, srv) in servers.iter().enumerate().skip(1) {
        assert_eq!(
            exchanges(&srv.api).await,
            before[i] + 3,
            "a text part, at {i}"
        );
    }
    // A protocol-1 part in one exchange is still served by this build.
    let (status, body) = send(
        &servers[1].api,
        "POST",
        "/peer/v1/part",
        json!({"protocol": 1, "index": "docs",
               "fts": pstore_format::text::FullText::default().encode(),
               "filters": null, "shadow": 0, "legs": [], "targets": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // A peer of M54's build (code review): it decodes the part before it checks the version,
    // and its legs have no `text` kind, so a phase-1 part with a text leg is `400 malformed
    // part` -- not the `409` the spec expected. Any status but 200 and 422 runs the share here.
    let fake = axum::Router::new().route(
        "/peer/v1/part",
        axum::routing::post(|body: axum::body::Bytes| async move {
            let v: Value = serde_json::from_slice(&body).unwrap();
            let text = v["legs"]
                .as_array()
                .is_some_and(|l| l.iter().any(|l| l["kind"] == "text"));
            if text {
                (axum::http::StatusCode::BAD_REQUEST, "malformed part")
            } else {
                (axum::http::StatusCode::CONFLICT, "protocol_mismatch")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(peer_fixture::serve_fake(listener, fake));
    let (store, servers, alone2) = cluster(std::slice::from_ref(&old)).await;
    let mut all = urls(&servers);
    all.push(old);
    fill(&store, &servers[0].api, &all, 8).await;
    let q = json!({"vector": Q, "text": "word2 tag1", "top_k": 10});
    let (_, want) = query(&alone2.api, &q).await;
    let failed = metric(&servers[0].api, "pstore_peer_parts_failed").await;
    let (s, got) = query(&servers[0].api, &q).await;
    assert_eq!(s, 200, "{got}");
    assert_eq!(answer(&got), answer(&want));
    assert_eq!(
        metric(&servers[0].api, "pstore_peer_parts_failed").await,
        failed + 1
    );
    let _ = alone;
}

#[tokio::test]
async fn a_failed_text_peer_costs_rounds_not_answers() {
    // Code review: a failure in each phase through the API. A dead peer fails phase 1; a peer
    // that opens its part (by passing it to a real server) and has lost it by its scan fails
    // phase 2 with the real server's `410`.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("https://{}", l.local_addr().unwrap())
    };
    let opener: Arc<Mutex<String>> = Arc::default();
    let fake = {
        let opener = Arc::clone(&opener);
        axum::Router::new().route(
            "/peer/v1/part",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let opener = opener.lock().unwrap().clone();
                    async move {
                        let v: Value = serde_json::from_slice(&body).unwrap();
                        if v["phase"] != "open" {
                            return (axum::http::StatusCode::GONE, Bytes::from("part_gone"));
                        }
                        let (status, body) = peer_fixture::call(
                            &opener,
                            headers["x-pstore-tenant"].to_str().unwrap(),
                            v,
                        )
                        .await;
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            Bytes::from(serde_json::to_vec(&body).unwrap()),
                        )
                    }
                },
            ),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lost = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(peer_fixture::serve_fake(listener, fake));
    let (store, servers, alone) = cluster(&[dead.clone(), lost.clone()]).await;
    *opener.lock().unwrap() = servers[1].url.clone();
    let mut all = urls(&servers);
    all.extend([dead, lost]);
    fill(&store, &servers[0].api, &all, 8).await;
    for (name, q) in queries()
        .into_iter()
        .filter(|(n, _)| n.starts_with("text") || n.starts_with("hybrid"))
    {
        let (_, want) = query(&alone.api, &q).await;
        let before = metric(&servers[0].api, "pstore_peer_parts_failed").await;
        let (s, got) = query(&servers[0].api, &q).await;
        assert_eq!(s, 200, "{name}: {got}");
        assert_eq!(answer(&got), answer(&want), "{name}");
        // The dead peer's part and the lost one's, each counted once.
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_failed").await,
            before + 2,
            "{name}"
        );
        // The lost part really was opened, by the server the fake passed it to.
        assert!(
            servers[1].view.take().iter().any(|k| k.ends_with(".tdict")),
            "{name}: nothing opened"
        );
    }
}

#[tokio::test]
async fn a_part_never_scanned_expires_without_more_traffic() {
    // Code review: expiry swept only on the next open or scan would leave a quiet peer holding
    // an abandoned part, uncounted, for as long as no query came. A scrape sweeps too.
    let (server, listener) = start(&MemoryStore::new(), 1).await;
    serve(&server, listener);
    let (status, body) = send(
        &server.api,
        "POST",
        "/peer/v1/part",
        json!({"protocol": 2, "phase": "open", "index": "docs",
               "fts": pstore_format::text::FullText::default().encode(),
               "filters": null, "shadow": 0, "legs": [], "targets": [], "terms": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(metric(&server.api, "pstore_peer_parts_expired").await, 0);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    assert_eq!(metric(&server.api, "pstore_peer_parts_expired").await, 1);
}

// ---- M56: the peer list from membership ----

const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| map.get(k).cloned()
}

#[test]
fn gossip_peers_are_configured_whole_and_never_beside_a_list() {
    use pstore_server::GossipConfig;
    let base = [
        ("PSTORE_PEER_SELF", "https://10.0.0.1:8080/"),
        ("PSTORE_PEER_GOSSIP_ADDR", "10.0.0.1:7000"),
        ("PSTORE_GOSSIP_KEY", KEY),
    ];
    let g = GossipConfig::from_vars(vars(&base)).unwrap().unwrap();
    assert_eq!(g.url, "https://10.0.0.1:8080");
    assert_eq!(
        (g.listen.as_str(), g.advertise.as_str()),
        ("10.0.0.1:7000", "10.0.0.1:7000")
    );
    assert!(g.seeds.is_empty());
    assert_eq!(g.period, Duration::from_millis(1_000));
    assert_eq!(g.keys.as_ref().map(Vec::len), Some(1));
    assert_eq!(g.timeout, Duration::from_millis(2_000));
    // ... and the static list is then not configured, nor refused for a lone PSTORE_PEER_SELF.
    assert_eq!(PeerConfig::from_vars(vars(&base)).unwrap(), None);

    let mut all = base.to_vec();
    all.extend([
        ("PSTORE_PEER_GOSSIP_ADDR", "0.0.0.0:7000"),
        ("PSTORE_PEER_GOSSIP_ADVERTISE", "[0:0:0:0:0:0:0:1]:7000"),
        (
            "PSTORE_PEER_GOSSIP_SEEDS",
            " 10.0.0.2:7000, [0:0:0:0:0:0:0:1]:7001 ,",
        ),
        ("PSTORE_PEER_GOSSIP_PERIOD_MS", "250"),
        ("PSTORE_PEER_TIMEOUT_MS", "900"),
    ]);
    let g = GossipConfig::from_vars(vars(&all)).unwrap().unwrap();
    assert_eq!(g.listen, "0.0.0.0:7000");
    // Normalised: two spellings of one address are one member.
    assert_eq!(g.advertise, "[::1]:7000");
    assert_eq!(
        g.seeds,
        vec!["10.0.0.2:7000".to_owned(), "[::1]:7001".to_owned()]
    );
    assert_eq!(g.period, Duration::from_millis(250));
    assert_eq!(g.timeout, Duration::from_millis(900));
    let insecure = [
        ("PSTORE_PEER_SELF", "https://10.0.0.1:8080"),
        ("PSTORE_PEER_GOSSIP_ADDR", "10.0.0.1:7000"),
        ("PSTORE_GOSSIP_INSECURE", "1"),
    ];
    assert!(
        GossipConfig::from_vars(vars(&insecure))
            .unwrap()
            .unwrap()
            .keys
            .is_none()
    );

    // None of it: no gossip, and M54's list the only way to have peers.
    assert_eq!(GossipConfig::from_vars(vars(&[])).unwrap(), None);
    let listed = [
        ("PSTORE_PEERS", "https://a:1,https://b:1"),
        ("PSTORE_PEER_SELF", "https://a:1"),
    ];
    assert_eq!(GossipConfig::from_vars(vars(&listed)).unwrap(), None);
    assert!(PeerConfig::from_vars(vars(&listed)).unwrap().is_some());
    assert!(
        PeerConfig::from_vars(vars(&[("PSTORE_PEER_SELF", "https://a:1")])).is_err(),
        "without gossip, a lone PSTORE_PEER_SELF is still refused"
    );

    let refused: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("beside a list", {
            let mut v = base.to_vec();
            v.push(("PSTORE_PEERS", "https://10.0.0.1:8080"));
            v
        }),
        (
            "no self",
            vec![
                ("PSTORE_PEER_GOSSIP_ADDR", "10.0.0.1:7000"),
                ("PSTORE_GOSSIP_KEY", KEY),
            ],
        ),
        ("plaintext self", {
            let mut v = base.to_vec();
            v[0] = ("PSTORE_PEER_SELF", "http://10.0.0.1:8080");
            v
        }),
        (
            "advertise alone",
            vec![("PSTORE_PEER_GOSSIP_ADVERTISE", "10.0.0.1:7000")],
        ),
        (
            "seeds alone",
            vec![("PSTORE_PEER_GOSSIP_SEEDS", "10.0.0.1:7000")],
        ),
        (
            "period alone",
            vec![("PSTORE_PEER_GOSSIP_PERIOD_MS", "100")],
        ),
        ("a hostname to listen on", {
            let mut v = base.to_vec();
            v[1] = ("PSTORE_PEER_GOSSIP_ADDR", "node-1:7000");
            v
        }),
        ("a hostname to advertise", {
            let mut v = base.to_vec();
            v.push(("PSTORE_PEER_GOSSIP_ADVERTISE", "node-1:7000"));
            v
        }),
        ("a hostname seed", {
            let mut v = base.to_vec();
            v.push(("PSTORE_PEER_GOSSIP_SEEDS", "10.0.0.2:7000,node-2:7000"));
            v
        }),
        ("an unspecified advertise", {
            let mut v = base.to_vec();
            v[1] = ("PSTORE_PEER_GOSSIP_ADDR", "0.0.0.0:7000");
            v
        }),
        ("an unspecified v6 advertise", {
            let mut v = base.to_vec();
            v.push(("PSTORE_PEER_GOSSIP_ADVERTISE", "[::]:7000"));
            v
        }),
        ("advertise port 0", {
            let mut v = base.to_vec();
            v[1] = ("PSTORE_PEER_GOSSIP_ADDR", "10.0.0.1:0");
            v
        }),
        ("period 0", {
            let mut v = base.to_vec();
            v.push(("PSTORE_PEER_GOSSIP_PERIOD_MS", "0"));
            v
        }),
        ("period not a number", {
            let mut v = base.to_vec();
            v.push(("PSTORE_PEER_GOSSIP_PERIOD_MS", "fast"));
            v
        }),
        ("no key", base[..2].to_vec()),
        ("a bad key", {
            let mut v = base.to_vec();
            v[2] = ("PSTORE_GOSSIP_KEY", "00");
            v
        }),
    ];
    for (name, v) in refused {
        assert!(
            GossipConfig::from_vars(vars(&v)).is_err(),
            "{name} was accepted"
        );
    }
}

#[test]
fn the_peer_list_is_the_servers_in_the_view() {
    let m = |addr: &str, zone: &str| (addr.to_owned(), zone.to_owned());
    let me = "https://10.0.0.2:8080";
    let view = vec![
        m("10.0.0.3:7000", "https://10.0.0.3:8080"),
        m("10.0.0.9:7000", "us-east-1a"),
        m("10.0.0.8:7000", ""),
        m("10.0.0.1:7000", "https://10.0.0.1:8080/"),
        m("10.0.0.1:7001", "https://10.0.0.1:8080"),
        m("10.0.0.6:7000", "http://10.0.0.6:8080"),
        m("10.0.0.7:7000", "https://10.0.0.7:8080/path"),
        m("[::1]:7000", "https://[::1]:8080"),
    ];
    let (list, at) = pstore_server::peer_list_of(&view, me);
    assert_eq!(
        list,
        vec![
            "https://10.0.0.1:8080".to_owned(),
            "https://10.0.0.2:8080".to_owned(),
            "https://10.0.0.3:8080".to_owned(),
            "https://[::1]:8080".to_owned(),
        ]
    );
    assert_eq!(at, 1);
    // Its own member present: still once.
    let mut with_me = view.clone();
    with_me.push(m("10.0.0.2:7000", me));
    assert_eq!(pstore_server::peer_list_of(&with_me, me), (list.clone(), 1));
    // Alone.
    assert_eq!(
        pstore_server::peer_list_of(&[], me),
        (vec![me.to_owned()], 0)
    );
}

/// A server with a gossip member of its own, at a 100 ms period, and an HTTP listener that
/// `stop` shuts.
struct Gossiping {
    server: Server,
    gossip: pstore_server::PeerGossip,
    http: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<std::io::Result<()>>,
}

const PERIOD: Duration = Duration::from_millis(100);

/// A free UDP port. ⚠️ Released before the member binds it, so another process could take it
/// in between: a small risk on a shared box, accepted as `free_port` in `pstore-node` does.
fn udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn gossiping(store: &MemoryStore, lane: u64, seeds: &[String]) -> (Gossiping, String) {
    let (server, (listener, udp)) = start(store, lane).await;
    let gossip_at = format!("127.0.0.1:{}", udp_port());
    let config = pstore_server::GossipConfig {
        url: server.url.clone(),
        listen: gossip_at.clone(),
        advertise: gossip_at.clone(),
        seeds: seeds.to_vec(),
        period: PERIOD,
        keys: Some(vec![vec![7u8; 32]]),
        timeout: Duration::from_secs(5),
    };
    let gossip = server.api.join_peers(config).await.unwrap();
    let (http, rx) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(pstore_server::serve_peers(
        Arc::clone(&server.api),
        listener,
        udp,
        async move {
            let _ = rx.await;
        },
    ));
    (
        Gossiping {
            server,
            gossip,
            http,
            serving,
        },
        gossip_at,
    )
}

/// Three gossiping servers, the second and third seeded with the first, and one alone.
async fn fleet() -> (MemoryStore, Vec<Gossiping>, Server) {
    let store = MemoryStore::new();
    let (first, at) = gossiping(&store, 1, &[]).await;
    let mut fleet = vec![first];
    for lane in 2..=3 {
        fleet.push(gossiping(&store, lane, std::slice::from_ref(&at)).await.0);
    }
    let (alone, _) = start(&store, 9).await;
    (store, fleet, alone)
}

/// Polls every `PERIOD` until `done`, for at most 40 periods.
async fn within_40_periods(mut done: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..40 {
        if done().await {
            return true;
        }
        tokio::time::sleep(PERIOD).await;
    }
    done().await
}

// ⚠️ Multi-threaded (code review): three members' tickers on one thread beside `fill`'s folds
// could miss probes for whole periods and suspect a live server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn servers_take_their_peers_from_membership() {
    let (store, fleet, alone) = fleet().await;
    let mut urls: Vec<String> = fleet.iter().map(|g| g.server.url.clone()).collect();
    urls.sort();
    let found =
        within_40_periods(async || fleet.iter().all(|g| g.server.api.peer_list() == urls)).await;
    assert!(
        found,
        "lists: {:?}",
        fleet
            .iter()
            .map(|g| g.server.api.peer_list())
            .collect::<Vec<_>>()
    );
    for g in &fleet {
        assert_eq!(metric(&g.server.api, "pstore_peer_servers").await, 3);
    }
    // What a log shows of a membership: its gossip address (from the sweep).
    let shown = format!("{:?}", fleet[0].gossip);
    assert!(
        shown.contains("PeerGossip") && shown.contains("127.0.0.1:"),
        "{shown}"
    );
    assert_eq!(metric(&alone.api, "pstore_peer_servers").await, 1);
    fill(&store, &fleet[0].server.api, &urls, 8).await;
    for q in [
        json!({"vector": Q, "top_k": 10}),
        json!({"text": "word2 tag1", "top_k": 10}),
    ] {
        let (_, want) = query(&alone.api, &q).await;
        for (i, g) in fleet.iter().enumerate() {
            let sent = metric(&g.server.api, "pstore_peer_parts_sent").await;
            let (s, got) = query(&g.server.api, &q).await;
            assert_eq!(s, 200, "{got}");
            assert_eq!(answer(&got), answer(&want), "{q} at {i}");
            assert!(
                metric(&g.server.api, "pstore_peer_parts_sent").await > sent,
                "{q} at {i} was not split"
            );
        }
    }
    for g in &fleet {
        assert_eq!(metric(&g.server.api, "pstore_peer_parts_failed").await, 0);
    }
    // Once only (code review): a second membership, or one beside a list, is refused.
    let again = pstore_server::GossipConfig {
        url: fleet[0].server.url.clone(),
        listen: format!("127.0.0.1:{}", udp_port()),
        advertise: format!("127.0.0.1:{}", udp_port()),
        seeds: vec![],
        period: PERIOD,
        keys: None,
        timeout: Duration::from_secs(5),
    };
    assert!(fleet[0].server.api.join_peers(again.clone()).await.is_err());
    alone
        .api
        .set_peers(Some(PeerConfig {
            servers: vec![alone.url.clone()],
            me: 0,
            timeout: Duration::from_secs(5),
        }))
        .unwrap();
    assert!(alone.api.join_peers(again).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_leaves_is_sent_nothing() {
    let (store, mut fleet, alone) = fleet().await;
    let mut urls: Vec<String> = fleet.iter().map(|g| g.server.url.clone()).collect();
    urls.sort();
    assert!(
        within_40_periods(async || fleet.iter().all(|g| g.server.api.peer_list() == urls)).await,
        "the fleet never found itself"
    );
    fill(&store, &fleet[0].server.api, &urls, 8).await;
    let q = json!({"vector": Q, "text": "word2 tag1", "top_k": 10});
    let (_, want) = query(&alone.api, &q).await;
    // The third leaves: its member and its HTTP listener both stop.
    let leaving = fleet.pop().unwrap();
    leaving.gossip.stop().await;
    let _ = leaving.http.send(());
    let gone = leaving.server.url.clone();
    // Its listener and every connection closed: awaited, never a sleep's guess (code review).
    tokio::time::timeout(Duration::from_secs(5), leaving.serving)
        .await
        .expect("the stopped server's HTTP did not shut down")
        .unwrap()
        .unwrap();
    // Before the others drop it, its share fails, and runs here.
    let failed = metric(&fleet[0].server.api, "pstore_peer_parts_failed").await;
    let (s, got) = query(&fleet[0].server.api, &q).await;
    assert_eq!(s, 200, "{got}");
    assert_eq!(answer(&got), answer(&want));
    assert!(
        metric(&fleet[0].server.api, "pstore_peer_parts_failed").await > failed,
        "a share to the stopped server did not fail"
    );
    assert!(
        within_40_periods(async || {
            fleet
                .iter()
                .all(|g| !g.server.api.peer_list().contains(&gone))
        })
        .await,
        "a stopped server was still listed after 40 periods"
    );
    let failed: Vec<u64> = futures_util::future::join_all(
        fleet
            .iter()
            .map(|g| metric(&g.server.api, "pstore_peer_parts_failed")),
    )
    .await;
    for _ in 0..10 {
        for g in &fleet {
            let sent = metric(&g.server.api, "pstore_peer_parts_sent").await;
            let (s, got) = query(&g.server.api, &q).await;
            assert_eq!(s, 200, "{got}");
            assert_eq!(answer(&got), answer(&want));
            assert!(
                metric(&g.server.api, "pstore_peer_parts_sent").await > sent,
                "not split"
            );
        }
    }
    for (g, before) in fleet.iter().zip(failed) {
        assert_eq!(
            metric(&g.server.api, "pstore_peer_parts_failed").await,
            before,
            "a share was still sent to the server that left"
        );
    }
    assert_eq!(metric(&fleet[0].server.api, "pstore_peer_servers").await, 2);
    drop(leaving.gossip);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_membership_leaves_the_list_too() {
    // From the sweep: dropping the handle, not only `stop`, must end the member -- or a server
    // that lost its handle on an error path would stay in every list, answering probes.
    let store = MemoryStore::new();
    let (first, at) = gossiping(&store, 1, &[]).await;
    let (second, _) = gossiping(&store, 2, std::slice::from_ref(&at)).await;
    assert!(
        within_40_periods(async || first.server.api.peer_list().len() == 2).await,
        "the two never met"
    );
    let gone = second.server.url.clone();
    drop(second.gossip);
    assert!(
        within_40_periods(async || !first.server.api.peer_list().contains(&gone)).await,
        "a dropped membership was still listed after 40 periods"
    );
}

#[tokio::test]
async fn a_sum_part_is_protocol_three() {
    // A peer that records each part's protocol and refuses 3, as a server of M55's build does.
    let seen: Arc<Mutex<Vec<(u64, String)>>> = Arc::default();
    let fake = {
        let seen = Arc::clone(&seen);
        axum::Router::new().route(
            "/peer/v1/part",
            axum::routing::post(move |body: axum::body::Bytes| {
                let seen = Arc::clone(&seen);
                async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    seen.lock().unwrap().push((
                        v["protocol"].as_u64().unwrap_or(0),
                        v["phase"].as_str().unwrap_or("").to_owned(),
                    ));
                    (axum::http::StatusCode::CONFLICT, "protocol_mismatch")
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(peer_fixture::serve_fake(listener, fake));
    let (store, servers, alone) = cluster(std::slice::from_ref(&old)).await;
    let mut all = urls(&servers);
    all.push(old);
    fill(&store, &servers[0].api, &all, 8).await;
    let q = json!({"text": ["word2 tag1", "common word3"], "top_k": 10,
                   "fusion": {"sum": {"weights": [2.0, 0.5]}}});
    let (_, want) = query(&alone.api, &q).await;
    let failed = metric(&servers[0].api, "pstore_peer_parts_failed").await;
    let (s, got) = query(&servers[0].api, &q).await;
    assert_eq!(s, 200, "{got}");
    assert_eq!(answer(&got), answer(&want));
    assert_eq!(
        metric(&servers[0].api, "pstore_peer_parts_failed").await,
        failed + 1
    );
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![(3, "open".to_owned())],
        "a sum part opens at protocol 3"
    );
    // And a real server refuses a cut by sum at protocol 2, and protocol 3 without one.
    let open = |protocol: u64, sum: Value| {
        json!({"protocol": protocol, "phase": "open", "index": "docs",
               "fts": pstore_format::text::FullText::default().encode(),
               "filters": null, "shadow": 0, "legs": [], "targets": [], "sum": sum})
    };
    let cut = json!({"weights": [2.0f32.to_bits(), 0.5f32.to_bits()], "keep": 10});
    for (protocol, sum) in [(2, cut.clone()), (3, Value::Null)] {
        let (status, body) = send(
            &servers[1].api,
            "POST",
            "/peer/v1/part",
            open(protocol, sum.clone()),
        )
        .await;
        assert_eq!(status, 400, "protocol {protocol}, sum {sum}: {body}");
    }
    let (status, body) = send(
        &servers[1].api,
        "POST",
        "/peer/v1/part",
        open(3, cut.clone()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // Code review: a leg numbered past MAX_LEGS, or twice, is refused before anything runs --
    // a cut by sum sizes its legs by that number.
    let leg =
        |j: u64| json!({"kind": "text", "j": j, "field": "text", "query": "word2", "limit": 1});
    for legs in [
        json!([leg(1_000_000_000_000)]),
        json!([leg(16)]),
        json!([leg(1), leg(1)]),
    ] {
        let mut part = open(3, cut.clone());
        part["legs"] = legs.clone();
        let (status, body) = send(&servers[1].api, "POST", "/peer/v1/part", part).await;
        assert_eq!(status, 400, "{legs}: {body}");
    }
    let mut part = open(3, cut.clone());
    part["legs"] = json!([leg(15)]);
    let (status, body) = send(&servers[1].api, "POST", "/peer/v1/part", part).await;
    assert_eq!(status, 200, "leg 15 is the last a query may have: {body}");
    // A cut by sum in one exchange is refused.
    let mut whole = open(1, cut.clone());
    whole.as_object_mut().unwrap().remove("phase");
    let (status, body) = send(&servers[1].api, "POST", "/peer/v1/part", whole).await;
    assert_eq!(status, 400, "protocol 1 with a cut: {body}");
    // A scan speaks its part's protocol.
    for (opened, scan) in [(3u64, 2u64), (2, 3)] {
        let sum = if opened == 3 {
            cut.clone()
        } else {
            Value::Null
        };
        let (status, body) =
            send(&servers[1].api, "POST", "/peer/v1/part", open(opened, sum)).await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = send(
            &servers[1].api,
            "POST",
            "/peer/v1/part",
            json!({"protocol": scan, "phase": "scan", "id": body["id"], "index": "docs",
                   "stats": {"doc_count": 0, "total_tokens": 0, "df": []}}),
        )
        .await;
        assert_eq!(status, 400, "opened at {opened}, scanned at {scan}: {body}");
    }
}

/// M154's rows: [`doc`]'s, each with a `title` declared English and stemming, so "running"
/// and "runs" are the term "run" in `title` and themselves in `text`.
fn titled(i: usize) -> Value {
    let mut d = doc(i);
    let head = ["run fast", "running late", "walk slowly", "runs daily"][i % 4];
    let tail = ["stories", "story told twice", "news"][i % 3];
    d["attributes"]["title"] = json!(format!("{head} {tail}"));
    d
}

fn title_schema() -> Value {
    json!({"title": {"type": "string",
                     "full_text_search": {"language": "english", "stemming": true}}})
}

/// M154 criterion 4's queries, each with the protocol its parts are sent at (criterion 5).
fn titled_queries() -> Vec<(&'static str, Value, u64)> {
    let over_title = json!(["title", "ContainsAnyToken", "Running"]);
    vec![
        (
            "over title",
            json!({"text": [{"field": "title", "query": "running stories"}], "top_k": 10}),
            5,
        ),
        (
            "sum over title",
            json!({"text": [{"field": "title", "query": "running"},
                            {"field": "title", "query": "runs story"}],
                   "top_k": 10, "fusion": {"sum": {"weights": [1.0, 0.5]}}}),
            6,
        ),
        (
            "vector, title filter",
            json!({"vector": Q, "top_k": 10, "filters": over_title}),
            4,
        ),
        (
            "text, title filter",
            json!({"text": "word2 tag1", "top_k": 10, "filters": over_title}),
            5,
        ),
    ]
}

/// The same index's queries that need no further field, and their protocols (criterion 5).
fn untitled_queries() -> Vec<(&'static str, Value, u64)> {
    vec![
        ("text", json!({"text": "word2 tag1", "top_k": 10}), 2),
        (
            "sum",
            json!({"text": ["word2 tag1", "common word3"], "top_k": 10,
                   "fusion": {"sum": {"weights": [2.0, 0.5]}}}),
            3,
        ),
        ("vector", json!({"vector": Q, "top_k": 10}), 1),
    ]
}

#[tokio::test]
async fn a_query_over_a_further_text_field_is_split_as_one_server_answers() {
    // M154 criterion 4: scored and filtered under `title`'s analyzer on every peer.
    let (store, servers, alone) = cluster(&[]).await;
    fill_declared(
        &store,
        &servers[0].api,
        &urls(&servers),
        8,
        titled,
        &title_schema(),
    )
    .await;
    for (name, q, _) in titled_queries() {
        let (s0, want) = query(&alone.api, &q).await;
        assert_eq!(s0, 200, "{name}: {want}");
        assert!(!want["results"].as_array().unwrap().is_empty(), "{name}");
        for (i, server) in servers.iter().enumerate() {
            let sent = metric(&server.api, "pstore_peer_parts_sent").await;
            let (s, got) = query(&server.api, &q).await;
            assert_eq!(s, 200, "{name} at {i}: {got}");
            assert_eq!(answer(&got), answer(&want), "{name} at {i}");
            assert!(
                metric(&server.api, "pstore_peer_parts_sent").await > sent,
                "{name} at {i} was not split"
            );
        }
    }
    for s in &servers {
        assert_eq!(metric(&s.api, "pstore_peer_parts_failed").await, 0);
    }
}

#[tokio::test]
async fn a_part_with_further_fields_is_its_own_protocol() {
    // M154 criterion 5: a peer that records each part's protocol and whether it carries
    // `texts`, and refuses everything, as a server of an earlier build refuses 4 to 6.
    let seen: Arc<Mutex<Vec<(u64, bool)>>> = Arc::default();
    let fake = {
        let seen = Arc::clone(&seen);
        axum::Router::new().route(
            "/peer/v1/part",
            axum::routing::post(move |body: axum::body::Bytes| {
                let seen = Arc::clone(&seen);
                async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    seen.lock().unwrap().push((
                        v["protocol"].as_u64().unwrap_or(0),
                        v.get("texts").is_some(),
                    ));
                    (axum::http::StatusCode::CONFLICT, "protocol_mismatch")
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(peer_fixture::serve_fake(listener, fake));
    let (store, servers, alone) = cluster(std::slice::from_ref(&old)).await;
    let mut all = urls(&servers);
    all.push(old);
    fill_declared(&store, &servers[0].api, &all, 8, titled, &title_schema()).await;
    for (name, q, protocol) in titled_queries().into_iter().chain(untitled_queries()) {
        let (_, want) = query(&alone.api, &q).await;
        let failed = metric(&servers[0].api, "pstore_peer_parts_failed").await;
        seen.lock().unwrap().clear();
        let (s, got) = query(&servers[0].api, &q).await;
        assert_eq!(s, 200, "{name}: {got}");
        assert_eq!(answer(&got), answer(&want), "{name}");
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_failed").await,
            failed + 1,
            "{name}"
        );
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![(protocol, protocol >= 4)],
            "{name}: sent at the wrong protocol"
        );
    }
}

#[tokio::test]
async fn a_part_with_further_fields_is_judged_at_the_door() {
    // M154 criterion 6, against a real server.
    let (_, servers, _) = cluster(&[]).await;
    let api = &servers[1].api;
    let fts = pstore_format::text::FullText::default().encode();
    let cut = json!({"weights": [1.0f32.to_bits()], "keep": 10});
    let texts = json!({"title": fts});
    let part = |protocol: u64, phase: Option<&str>, texts: &Value, sum: &Value| {
        let mut p = json!({"protocol": protocol, "index": "docs", "fts": fts,
                           "filters": null, "shadow": 0, "legs": [], "targets": []});
        if let Some(phase) = phase {
            p["phase"] = json!(phase);
        }
        if !texts.is_null() {
            p["texts"] = texts.clone();
        }
        if !sum.is_null() {
            p["sum"] = sum.clone();
        }
        p
    };
    let post = |body: Value| send(api, "POST", "/peer/v1/part", body);
    let none = Value::Null;
    let nine: serde_json::Map<String, Value> =
        (0..9).map(|i| (format!("f{i}"), json!(fts))).collect();
    let refused = [
        (
            "open at 5 without texts",
            part(5, Some("open"), &none, &none),
        ),
        (
            "open at 6 without texts",
            part(6, Some("open"), &none, &cut),
        ),
        ("whole at 4 without texts", part(4, None, &none, &none)),
        ("whole at 1 with texts", part(1, None, &texts, &none)),
        ("open at 2 with texts", part(2, Some("open"), &texts, &none)),
        ("open at 3 with texts", part(3, Some("open"), &texts, &cut)),
        (
            "open at 5, texts that do not decode",
            part(5, Some("open"), &json!({"title": "v9;nope"}), &none),
        ),
        (
            "open at 5, nine texts",
            part(5, Some("open"), &Value::Object(nine), &none),
        ),
        ("open at 5 with a sum", part(5, Some("open"), &texts, &cut)),
        (
            "open at 6 without a sum",
            part(6, Some("open"), &texts, &none),
        ),
        ("whole at 4 with a sum", part(4, None, &texts, &cut)),
    ];
    for (name, body) in refused {
        let (status, got) = post(body).await;
        assert_eq!(status, 400, "{name}: {got}");
    }
    let (status, got) = post(part(4, None, &texts, &none)).await;
    assert_eq!(status, 200, "whole at 4: {got}");
    // The bound itself: eight further fields are accepted (CI's mutation shard 10 on PR #8).
    let eight: serde_json::Map<String, Value> =
        (0..8).map(|i| (format!("f{i}"), json!(fts))).collect();
    let (status, got) = post(part(4, None, &Value::Object(eight), &none)).await;
    assert_eq!(status, 200, "whole at 4 with eight texts: {got}");
    // A scan speaks its part's protocol; each scan takes its part, so each opens its own.
    let opened_at = |protocol: u64| {
        let (t, c) = match protocol {
            2 => (&none, &none),
            5 => (&texts, &none),
            _ => (&texts, &cut),
        };
        part(protocol, Some("open"), t, c)
    };
    let scan = |protocol: u64, id: &Value| {
        json!({"protocol": protocol, "phase": "scan", "id": id, "index": "docs",
               "stats": {"doc_count": 0, "total_tokens": 0, "df": []}})
    };
    for (opened, scanned, want) in [
        (5, 2, 400),
        (5, 3, 400),
        (5, 6, 400),
        (2, 5, 400),
        (6, 3, 400),
        (6, 5, 400),
        (5, 5, 200),
        (6, 6, 200),
    ] {
        let (status, got) = post(opened_at(opened)).await;
        assert_eq!(status, 200, "open at {opened}: {got}");
        let (status, got) = post(scan(scanned, &got["id"])).await;
        assert_eq!(
            status, want,
            "opened at {opened}, scanned at {scanned}: {got}"
        );
    }
}

/// M157's rows: [`titled`]'s, each with `tags`, an array whose "word" shares a word with
/// `text` at another `df`.
fn fielded(i: usize) -> Value {
    let mut d = titled(i);
    let tag = ["alpha", "beta", "gamma"][i % 3];
    d["attributes"]["tags"] = json!([tag, format!("word{}", i % 4)]);
    d
}

fn fielded_schema() -> Value {
    let mut s = title_schema();
    s["tags"] = json!({"type": "[]string", "full_text_search": true});
    s
}

/// M157 criterion 3's queries, each with the protocol its parts are sent at (criterion 4).
fn fielded_queries() -> Vec<(&'static str, Value, u64)> {
    let two = json!([{"field": "text", "query": "word2 tag1"},
                     {"field": "title", "query": "running stories"}]);
    vec![
        ("text and title", json!({"text": two, "top_k": 10}), 7),
        (
            "sum over three fields",
            json!({"text": [{"field": "text", "query": "word2 tag1"},
                            {"field": "title", "query": "running"},
                            {"field": "tags", "query": "word2 alpha"}],
                   "top_k": 10, "fusion": {"sum": {"weights": [1.0, 0.5, 2.0]}}}),
            8,
        ),
        (
            "text and title, title filter",
            json!({"text": two, "top_k": 10,
                   "filters": ["title", "ContainsAnyToken", "Running"]}),
            7,
        ),
    ]
}

#[tokio::test]
async fn a_query_over_several_text_fields_is_split_as_one_server_answers() {
    // M157 criterion 3: each field scored by its own statistics on every peer.
    let (store, servers, alone) = cluster(&[]).await;
    fill_declared(
        &store,
        &servers[0].api,
        &urls(&servers),
        8,
        fielded,
        &fielded_schema(),
    )
    .await;
    for (name, q, _) in fielded_queries() {
        let (s0, want) = query(&alone.api, &q).await;
        assert_eq!(s0, 200, "{name}: {want}");
        assert!(!want["results"].as_array().unwrap().is_empty(), "{name}");
        for (i, server) in servers.iter().enumerate() {
            let sent = metric(&server.api, "pstore_peer_parts_sent").await;
            let (s, got) = query(&server.api, &q).await;
            assert_eq!(s, 200, "{name} at {i}: {got}");
            assert_eq!(answer(&got), answer(&want), "{name} at {i}");
            assert!(
                metric(&server.api, "pstore_peer_parts_sent").await > sent,
                "{name} at {i} was not split"
            );
        }
    }
    for s in &servers {
        assert_eq!(metric(&s.api, "pstore_peer_parts_failed").await, 0);
    }
}

#[tokio::test]
async fn a_part_over_several_fields_is_its_own_protocol() {
    // M157 criterion 4: a peer that records each part's protocol, and whether it carries
    // `field_terms` and `terms`, and refuses everything, as a server of an earlier build
    // refuses 7 and 8.
    let seen: Arc<Mutex<Vec<(u64, usize, bool)>>> = Arc::default();
    let fake = {
        let seen = Arc::clone(&seen);
        axum::Router::new().route(
            "/peer/v1/part",
            axum::routing::post(move |body: axum::body::Bytes| {
                let seen = Arc::clone(&seen);
                async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    seen.lock().unwrap().push((
                        v["protocol"].as_u64().unwrap_or(0),
                        v.get("field_terms")
                            .and_then(Value::as_object)
                            .map_or(0, serde_json::Map::len),
                        v.get("terms").is_some(),
                    ));
                    (axum::http::StatusCode::CONFLICT, "protocol_mismatch")
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(peer_fixture::serve_fake(listener, fake));
    let (store, servers, alone) = cluster(std::slice::from_ref(&old)).await;
    let mut all = urls(&servers);
    all.push(old);
    fill_declared(&store, &servers[0].api, &all, 8, fielded, &fielded_schema()).await;
    let one_field = vec![
        (
            "title alone",
            json!({"text": [{"field": "title", "query": "running stories"}], "top_k": 10}),
            5,
        ),
        ("text alone", json!({"text": "word2 tag1", "top_k": 10}), 2),
    ];
    for (name, q, protocol) in fielded_queries().into_iter().chain(one_field) {
        let (_, want) = query(&alone.api, &q).await;
        let failed = metric(&servers[0].api, "pstore_peer_parts_failed").await;
        seen.lock().unwrap().clear();
        let (s, got) = query(&servers[0].api, &q).await;
        assert_eq!(s, 200, "{name}: {got}");
        assert_eq!(answer(&got), answer(&want), "{name}");
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_failed").await,
            failed + 1,
            "{name}"
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{name}: {seen:?}");
        let (p, fields, terms) = seen[0];
        assert_eq!(p, protocol, "{name}: sent at the wrong protocol");
        if protocol >= 7 {
            assert!(fields >= 2 && !terms, "{name}: {seen:?}");
        } else {
            assert!(fields == 0 && terms, "{name}: {seen:?}");
        }
    }
    // A query over the primary and a field the index never declared sends no part: its
    // refusal is the footers', not a peer's.
    seen.lock().unwrap().clear();
    let q = json!({"text": [{"field": "text", "query": "word2"},
                            {"field": "nosuch", "query": "word2"}], "top_k": 10});
    let (s, got) = query(&servers[0].api, &q).await;
    assert_eq!(s, 400, "{got}");
    assert_eq!(got["error"]["code"], "unknown_field", "{got}");
    assert!(seen.lock().unwrap().is_empty(), "a part was sent");
}

#[tokio::test]
async fn a_phase_one_reply_short_of_a_field_is_a_failed_share() {
    // M157 criterion 4: a peer that forwards each part to a real server and answers with its
    // reply, but for one field left out of `field_stats` -- or, at 5, no `stats`.
    let real: Arc<std::sync::OnceLock<String>> = Arc::default();
    let fake = {
        let real = Arc::clone(&real);
        axum::Router::new().route(
            "/peer/v1/part",
            axum::routing::post(move |body: axum::body::Bytes| {
                let real = Arc::clone(&real);
                async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    let (status, mut reply) =
                        peer_fixture::call(real.get().unwrap(), "7", v.clone()).await;
                    if v["phase"] == "open" && status == 200 {
                        if let Some(f) = reply.get_mut("field_stats").and_then(Value::as_object_mut)
                        {
                            let first = f.keys().next().unwrap().clone();
                            f.remove(&first);
                        } else if v["protocol"] == 5 {
                            reply.as_object_mut().unwrap().remove("stats");
                        }
                    }
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        axum::Json(reply),
                    )
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let short = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(peer_fixture::serve_fake(listener, fake));
    let (store, servers, alone) = cluster(std::slice::from_ref(&short)).await;
    real.set(
        peer_addresses()
            .lock()
            .unwrap()
            .get(&(Arc::as_ptr(&servers[1].api) as usize))
            .unwrap()
            .clone(),
    )
    .unwrap();
    let mut all = urls(&servers);
    all.push(short);
    fill_declared(&store, &servers[0].api, &all, 8, fielded, &fielded_schema()).await;
    let cases = [
        fielded_queries().swap_remove(0).1,
        json!({"text": [{"field": "title", "query": "running stories"}], "top_k": 10}),
    ];
    for q in cases {
        let (_, want) = query(&alone.api, &q).await;
        let failed = metric(&servers[0].api, "pstore_peer_parts_failed").await;
        let (s, got) = query(&servers[0].api, &q).await;
        assert_eq!(s, 200, "{q}: {got}");
        assert_eq!(answer(&got), answer(&want), "{q}");
        assert_eq!(
            metric(&servers[0].api, "pstore_peer_parts_failed").await,
            failed + 1,
            "{q}"
        );
    }
}

#[tokio::test]
async fn a_part_over_several_fields_is_judged_at_the_door() {
    // M157 criterion 5, against a real server.
    let (_, servers, _) = cluster(&[]).await;
    let api = &servers[1].api;
    let fts = pstore_format::text::FullText::default().encode();
    let cut = json!({"weights": [1.0f32.to_bits(), 1.0f32.to_bits()], "keep": 10});
    let leg = |j: usize, field: &str| json!({"kind": "text", "j": j, "field": field, "query": "word2", "limit": 10});
    let legs_over = |fields: &[&str]| -> Value {
        fields
            .iter()
            .enumerate()
            .map(|(j, f)| leg(j, f))
            .collect::<Vec<_>>()
            .into()
    };
    let terms_of = |fields: &[&str]| -> Value {
        fields
            .iter()
            .map(|f| ((*f).to_owned(), json!(["word2"])))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let texts = json!({"title": fts});
    let two = ["text", "title"];
    // A part at `protocol`, its legs over `legs`, with each optional key set when not null.
    let part = |protocol: u64, phase: Option<&str>, legs: &[&str], keys: &[(&str, Value)]| {
        let mut p = json!({"protocol": protocol, "index": "docs", "fts": fts,
                           "filters": null, "shadow": 0, "legs": legs_over(legs),
                           "targets": []});
        if let Some(phase) = phase {
            p["phase"] = json!(phase);
        }
        for (k, v) in keys {
            p[*k] = v.clone();
        }
        p
    };
    let post = |body: Value| send(api, "POST", "/peer/v1/part", body);
    let ten: Vec<String> = (0..10).map(|i| format!("f{i}")).collect();
    let ten: Vec<&str> = ten.iter().map(String::as_str).collect();
    let open7 = |legs: &[&str], keys: &[(&str, Value)]| part(7, Some("open"), legs, keys);
    let well = |fields: &[&str]| vec![("field_terms", terms_of(fields)), ("texts", texts.clone())];
    let refused = [
        (
            "7 with terms",
            open7(
                &two,
                &[
                    ("field_terms", terms_of(&two)),
                    ("texts", texts.clone()),
                    ("terms", json!(["word2"])),
                ],
            ),
        ),
        (
            "7 without field_terms",
            open7(
                &two,
                &[("texts", texts.clone()), ("terms", json!(["word2"]))],
            ),
        ),
        ("7 with one field", open7(&["title"], &well(&["title"]))),
        ("7 with ten fields", open7(&ten, &well(&ten))),
        (
            "7 without texts",
            open7(&two, &[("field_terms", terms_of(&two))]),
        ),
        (
            "7, a field no leg reads",
            open7(&two, &well(&["tags", "text", "title"])),
        ),
        (
            "7, a leg over a field not named",
            open7(&["tags", "text", "title"], &well(&two)),
        ),
        ("7 with a sum", {
            let mut k = well(&two);
            k.push(("sum", cut.clone()));
            open7(&two, &k)
        }),
        ("8 without a sum", part(8, Some("open"), &two, &well(&two))),
        (
            "2 with field_terms",
            part(
                2,
                Some("open"),
                &["text"],
                &[("terms", json!(["word2"])), ("field_terms", terms_of(&two))],
            ),
        ),
        (
            "1 with field_terms",
            part(1, None, &[], &[("field_terms", terms_of(&two))]),
        ),
        (
            "5 over two fields",
            part(
                5,
                Some("open"),
                &two,
                &[("terms", json!(["word2"])), ("texts", texts.clone())],
            ),
        ),
    ];
    for (name, body) in refused {
        let (status, got) = post(body).await;
        assert_eq!(status, 400, "{name}: {got}");
    }
    // The bound itself: the primary and eight further fields, nine in all, are accepted (CI's
    // mutation shard 11 on PR #8).
    let nine: Vec<String> = std::iter::once("text".to_owned())
        .chain((0..8).map(|i| format!("f{i}")))
        .collect();
    let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
    let eight: serde_json::Map<String, Value> = nine[1..]
        .iter()
        .map(|f| ((*f).to_owned(), json!(fts)))
        .collect();
    let (status, got) = post(open7(
        &nine,
        &[
            ("field_terms", terms_of(&nine)),
            ("texts", Value::Object(eight)),
        ],
    ))
    .await;
    assert_eq!(status, 200, "7 with nine fields: {got}");
    let opened_at = |protocol: u64| match protocol {
        2 => part(2, Some("open"), &["text"], &[("terms", json!(["word2"]))]),
        5 => part(
            5,
            Some("open"),
            &["title"],
            &[("terms", json!(["word2"])), ("texts", texts.clone())],
        ),
        6 => part(
            6,
            Some("open"),
            &["title", "title"],
            &[
                ("terms", json!(["word2"])),
                ("texts", texts.clone()),
                ("sum", cut.clone()),
            ],
        ),
        7 => open7(&two, &well(&two)),
        _ => {
            let mut k = well(&two);
            k.push(("sum", cut.clone()));
            part(8, Some("open"), &two, &k)
        }
    };
    let stats = json!({"doc_count": 0, "total_tokens": 0, "df": []});
    let per_field = |fields: &[&str]| -> Value {
        fields
            .iter()
            .map(|f| ((*f).to_owned(), stats.clone()))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let scan = |protocol: u64, id: &Value, keys: &[(&str, Value)]| {
        let mut s = json!({"protocol": protocol, "phase": "scan", "id": id, "index": "docs"});
        for (k, v) in keys {
            s[*k] = v.clone();
        }
        s
    };
    let fields = |p: u64| -> Vec<(&str, Value)> {
        if p >= 7 {
            vec![("field_stats", per_field(&two))]
        } else {
            vec![("stats", stats.clone())]
        }
    };
    // Opened at, scanned at, the scan's statistics, and the status it is answered.
    type Case<'a> = (u64, u64, Vec<(&'a str, Value)>, u16);
    let mut cases: Vec<Case<'_>> = Vec::new();
    for (opened, scanned) in [(8, 7), (7, 8), (5, 7), (6, 8), (7, 5), (8, 6)] {
        cases.push((opened, scanned, fields(scanned), 400));
    }
    cases.extend([
        (
            7,
            7,
            vec![("field_stats", per_field(&["tags", "text"]))],
            400,
        ),
        (7, 7, vec![("field_stats", per_field(&["title"]))], 400),
        (
            7,
            7,
            vec![("field_stats", per_field(&two)), ("stats", stats.clone())],
            400,
        ),
        (
            2,
            2,
            vec![
                ("stats", stats.clone()),
                ("field_stats", per_field(&["text"])),
            ],
            400,
        ),
        (2, 2, vec![], 400),
        (7, 7, fields(7), 200),
        (8, 8, fields(8), 200),
        (2, 2, fields(2), 200),
    ]);
    for (opened, scanned, keys, want) in cases {
        let (status, got) = post(opened_at(opened)).await;
        assert_eq!(status, 200, "open at {opened}: {got}");
        if opened >= 7 {
            assert_eq!(
                got["field_stats"].as_object().map(serde_json::Map::len),
                Some(2),
                "open at {opened}: {got}"
            );
            assert!(got.get("stats").is_none(), "open at {opened}: {got}");
        }
        let body = scan(scanned, &got["id"], &keys);
        let (status, got) = post(body.clone()).await;
        assert_eq!(status, want, "opened at {opened}: {body} -> {got}");
    }
}
