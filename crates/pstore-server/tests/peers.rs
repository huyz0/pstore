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

/// A server over `store`, listening on a port of its own.
async fn start(store: &MemoryStore, lane: u64) -> (Server, tokio::net::TcpListener) {
    let view = View::over(store);
    let api = Api::new(Accounted::new(view.clone()), LaneId(lane)).unwrap();
    api.set_index_params_for_test(params());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (Server { api, view, url }, listener)
}

fn serve(server: &Server, listener: tokio::net::TcpListener) {
    tokio::spawn(pstore_server::serve(
        Arc::clone(&server.api),
        listener,
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
    let mut k = 0;
    loop {
        let docs: Vec<Value> = (k * 24..k * 24 + 24).map(doc).collect();
        let (s, b) = send(
            api,
            "PUT",
            "/v1/indexes/docs/documents",
            json!({"durability": "durable", "documents": docs}),
        )
        .await;
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
        .find_map(|l| {
            l.strip_prefix(
                "pstore_http_requests_total{route=\"/v1/internal/part\",status=\"200\"} ",
            )
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
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
    // ⚠️ Wall clock under an injected wait per request: four rounds of 250 ms is a second,
    // and a chain of five -- the peers called only after the coordinator's own open -- is at
    // least 1.25 s. A per-store round counter cannot tell the two apart: each store sees its
    // own rounds, not the chain.
    let (store, servers, _) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let q = json!({"vector": Q, "top_k": 10});
    // Warm nothing; but make sure the query itself is sound first.
    let sent = metric(&servers[0].api, "pstore_peer_parts_sent").await;
    for s in &servers {
        s.view.delay_ms.store(250, Ordering::SeqCst);
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
        took < Duration::from_millis(1_125),
        "a split query took {took:?}: more than four rounds of 250 ms"
    );
}

#[tokio::test]
async fn a_failed_peer_costs_rounds_not_answers() {
    // A server in the list that is not listening: its segments' parts fail to connect.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
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
async fn a_peer_of_another_protocol_is_refused_and_run_here() {
    // A peer that answers every part with the version refusal.
    let fake = axum::Router::new().route(
        "/v1/internal/part",
        axum::routing::post(|| async { (axum::http::StatusCode::CONFLICT, "protocol_mismatch") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, fake).await });
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
        "/v1/internal/part",
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
            "/v1/internal/part",
            json!({"protocol": 1, "phase": phase, "index": "docs", "fts": "", "filters": null,
                   "shadow": 0, "legs": [], "targets": [], "id": "00", "stats": null}),
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
        let (status, body) = send(&s.api, "POST", "/v1/internal/part", p.clone()).await;
        assert_eq!(status, 400, "{p} -> {body}");
        assert!(
            s.view.take().is_empty(),
            "a refused part read the store: {p}"
        );
    }
    // An index named `a..b` is one of the tenant's: `..` refuses only as a path component.
    let dotted = "0007/tnt/7/idx/a..b/seg/L0/00000000000000000001-0000000000000001.seg";
    let (status, body) = send(&s.api, "POST", "/v1/internal/part", part(dotted, None, 1)).await;
    assert_ne!(status, 400, "{body}");
    // The same part with its own keys is admitted, and reads.
    let (status, body) = send(
        &s.api,
        "POST",
        "/v1/internal/part",
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
        ("PSTORE_PEERS", "http://a:1, http://b:1/,http://c:1"),
        ("PSTORE_PEER_SELF", "http://b:1/"),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(ok.servers, ["http://a:1", "http://b:1", "http://c:1"]);
    assert_eq!(ok.me, 1);
    assert_eq!(ok.timeout, Duration::from_millis(2_000));
    let timed = read(&[
        ("PSTORE_PEERS", "http://a:1,http://b:1"),
        ("PSTORE_PEER_SELF", "http://a:1"),
        ("PSTORE_PEER_TIMEOUT_MS", "150"),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(timed.timeout, Duration::from_millis(150));
    for bad in [
        vec![("PSTORE_PEERS", "http://a:1,http://b:1")],
        vec![("PSTORE_PEER_SELF", "http://a:1")],
        vec![
            ("PSTORE_PEERS", "http://a:1,http://b:1"),
            ("PSTORE_PEER_SELF", "http://c:1"),
        ],
        vec![
            ("PSTORE_PEERS", "http://a:1,http://a:1/"),
            ("PSTORE_PEER_SELF", "http://a:1"),
        ],
        vec![
            ("PSTORE_PEERS", "http://a:1"),
            ("PSTORE_PEER_SELF", "http://a:1"),
            ("PSTORE_PEER_TIMEOUT_MS", "0"),
        ],
        vec![("PSTORE_PEER_TIMEOUT_MS", "soon")],
        vec![
            ("PSTORE_PEERS", "https://a:1,http://b:1"),
            ("PSTORE_PEER_SELF", "http://b:1"),
        ],
        vec![
            ("PSTORE_PEERS", "http://a:1/v1,http://b:1"),
            ("PSTORE_PEER_SELF", "http://b:1"),
        ],
        vec![
            ("PSTORE_PEERS", "a:1,http://b:1"),
            ("PSTORE_PEER_SELF", "http://b:1"),
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
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
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
    // round trip between servers, never a blob round.
    let (store, servers, _) = cluster(&[]).await;
    fill(&store, &servers[0].api, &urls(&servers), 8).await;
    let q = json!({"vector": Q, "text": "word2 tag1", "top_k": 10});
    let sent = metric(&servers[0].api, "pstore_peer_parts_sent").await;
    for s in &servers {
        s.view.delay_ms.store(250, Ordering::SeqCst);
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
        took < Duration::from_millis(1_125),
        "a split text query took {took:?}: more than four rounds of 250 ms"
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
        "/v1/internal/part",
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
        "/v1/internal/part",
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
    let old = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, fake).await });
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
        format!("http://{}", l.local_addr().unwrap())
    };
    let opener: Arc<Mutex<String>> = Arc::default();
    let fake = {
        let opener = Arc::clone(&opener);
        axum::Router::new().route(
            "/v1/internal/part",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let opener = opener.lock().unwrap().clone();
                    async move {
                        let v: Value = serde_json::from_slice(&body).unwrap();
                        if v["phase"] != "open" {
                            return (axum::http::StatusCode::GONE, Bytes::from("part_gone"));
                        }
                        let res = reqwest::Client::builder()
                            .no_proxy()
                            .build()
                            .unwrap()
                            .post(format!("{opener}/v1/internal/part"))
                            .header("x-pstore-tenant", headers["x-pstore-tenant"].clone())
                            .header("content-type", "application/json")
                            .body(body)
                            .send()
                            .await
                            .unwrap();
                        let status =
                            axum::http::StatusCode::from_u16(res.status().as_u16()).unwrap();
                        (status, res.bytes().await.unwrap())
                    }
                },
            ),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lost = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, fake).await });
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

#[tokio::test(start_paused = true)]
async fn a_part_never_scanned_expires_without_more_traffic() {
    // Code review: expiry swept only on the next open or scan would leave a quiet peer holding
    // an abandoned part, uncounted, for as long as no query came. A scrape sweeps too.
    let (server, _listener) = start(&MemoryStore::new(), 1).await;
    let (status, body) = send(
        &server.api,
        "POST",
        "/v1/internal/part",
        json!({"protocol": 2, "phase": "open", "index": "docs",
               "fts": pstore_format::text::FullText::default().encode(),
               "filters": null, "shadow": 0, "legs": [], "targets": [], "terms": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(metric(&server.api, "pstore_peer_parts_expired").await, 0);
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
        ("PSTORE_PEER_SELF", "http://10.0.0.1:8080/"),
        ("PSTORE_PEER_GOSSIP_ADDR", "10.0.0.1:7000"),
        ("PSTORE_GOSSIP_KEY", KEY),
    ];
    let g = GossipConfig::from_vars(vars(&base)).unwrap().unwrap();
    assert_eq!(g.url, "http://10.0.0.1:8080");
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
        ("PSTORE_PEER_SELF", "http://10.0.0.1:8080"),
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
        ("PSTORE_PEERS", "http://a:1,http://b:1"),
        ("PSTORE_PEER_SELF", "http://a:1"),
    ];
    assert_eq!(GossipConfig::from_vars(vars(&listed)).unwrap(), None);
    assert!(PeerConfig::from_vars(vars(&listed)).unwrap().is_some());
    assert!(
        PeerConfig::from_vars(vars(&[("PSTORE_PEER_SELF", "http://a:1")])).is_err(),
        "without gossip, a lone PSTORE_PEER_SELF is still refused"
    );

    let refused: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("beside a list", {
            let mut v = base.to_vec();
            v.push(("PSTORE_PEERS", "http://10.0.0.1:8080"));
            v
        }),
        (
            "no self",
            vec![
                ("PSTORE_PEER_GOSSIP_ADDR", "10.0.0.1:7000"),
                ("PSTORE_GOSSIP_KEY", KEY),
            ],
        ),
        ("https self", {
            let mut v = base.to_vec();
            v[0] = ("PSTORE_PEER_SELF", "https://10.0.0.1:8080");
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
    let me = "http://10.0.0.2:8080";
    let view = vec![
        m("10.0.0.3:7000", "http://10.0.0.3:8080"),
        m("10.0.0.9:7000", "us-east-1a"),
        m("10.0.0.8:7000", ""),
        m("10.0.0.1:7000", "http://10.0.0.1:8080/"),
        m("10.0.0.1:7001", "http://10.0.0.1:8080"),
        m("10.0.0.6:7000", "https://10.0.0.6:8080"),
        m("10.0.0.7:7000", "http://10.0.0.7:8080/path"),
        m("[::1]:7000", "http://[::1]:8080"),
    ];
    let (list, at) = pstore_server::peer_list_of(&view, me);
    assert_eq!(
        list,
        vec![
            "http://10.0.0.1:8080".to_owned(),
            "http://10.0.0.2:8080".to_owned(),
            "http://10.0.0.3:8080".to_owned(),
            "http://[::1]:8080".to_owned(),
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
    let (server, listener) = start(store, lane).await;
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
    let serving = tokio::spawn(pstore_server::serve(
        Arc::clone(&server.api),
        listener,
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
            "/v1/internal/part",
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
    let old = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, fake).await });
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
            "/v1/internal/part",
            open(protocol, sum.clone()),
        )
        .await;
        assert_eq!(status, 400, "protocol {protocol}, sum {sum}: {body}");
    }
    let (status, body) = send(
        &servers[1].api,
        "POST",
        "/v1/internal/part",
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
        let (status, body) = send(&servers[1].api, "POST", "/v1/internal/part", part).await;
        assert_eq!(status, 400, "{legs}: {body}");
    }
    let mut part = open(3, cut.clone());
    part["legs"] = json!([leg(15)]);
    let (status, body) = send(&servers[1].api, "POST", "/v1/internal/part", part).await;
    assert_eq!(status, 200, "leg 15 is the last a query may have: {body}");
    // A cut by sum in one exchange is refused.
    let mut whole = open(1, cut.clone());
    whole.as_object_mut().unwrap().remove("phase");
    let (status, body) = send(&servers[1].api, "POST", "/v1/internal/part", whole).await;
    assert_eq!(status, 400, "protocol 1 with a cut: {body}");
    // A scan speaks its part's protocol.
    for (opened, scan) in [(3u64, 2u64), (2, 3)] {
        let sum = if opened == 3 {
            cut.clone()
        } else {
            Value::Null
        };
        let (status, body) = send(
            &servers[1].api,
            "POST",
            "/v1/internal/part",
            open(opened, sum),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = send(
            &servers[1].api,
            "POST",
            "/v1/internal/part",
            json!({"protocol": scan, "phase": "scan", "id": body["id"], "index": "docs",
                   "stats": {"doc_count": 0, "total_tokens": 0, "df": []}}),
        )
        .await;
        assert_eq!(status, 400, "opened at {opened}, scanned at {scan}: {body}");
    }
}
