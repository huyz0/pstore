//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `session` consistency (M11.1): a token minted by writes and echoed by the client makes a
//! read reflect the session's own durable writes, and never go backwards.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition,
    PutOutcome,
};
use pstore_server::{Api, FoldPolicy};
use pstore_types::CasTag;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const TENANT: u128 = 19;
const HEADER: &str = "x-pstore-session";

type A = Arc<Api<MemoryStore>>;

fn apis(lanes: &[u64]) -> Vec<A> {
    let store = Accounted::new(MemoryStore::new());
    lanes
        .iter()
        .map(|l| Api::new(store.clone(), LaneId(*l)).unwrap())
        .collect()
}

async fn send(api: &A, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

fn request(
    tenant: u128,
    method: &str,
    uri: &str,
    body: &Value,
    token: Option<&str>,
) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string());
    if let Some(t) = token {
        r = r.header(HEADER, t);
    }
    r.body(Body::from(body.to_string())).unwrap()
}

/// Writes `id`, durably or batched, carrying `token`; returns the token the write minted.
async fn write(api: &A, id: &str, durable: bool, token: Option<&str>) -> String {
    let body = json!({
        "durability": if durable { "durable" } else { "batched" },
        "documents": [{"id": id, "vector": [1.0, 0.5]}]
    });
    let (s, h, b) = send(
        api,
        request(TENANT, "PUT", "/v1/indexes/docs/documents", &body, token),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let t = h[HEADER].to_str().unwrap().to_owned();
    assert_eq!(b["session"], json!(t), "the body echoes the header");
    t
}

fn q(consistency: &str) -> Value {
    json!({"vector": [1.0, 0.5], "top_k": 100, "consistency": consistency})
}

async fn query(api: &A, body: &Value, token: Option<&str>) -> (StatusCode, HeaderMap, Value) {
    send(
        api,
        request(TENANT, "POST", "/v1/indexes/docs/query", body, token),
    )
    .await
}

fn ids(b: &Value) -> Vec<String> {
    let mut v: Vec<String> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    v.sort();
    v
}

fn requested_only() -> FoldPolicy {
    FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::from_secs(3600),
        bytes: 1 << 40,
    }
}

fn decode(t: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(t)
        .unwrap()
}

/// A token laid out as the spec states it.
fn token(version: u8, tenant: u128, epoch: u64, flags: u8, entries: &[(u64, u64)]) -> String {
    let mut b = vec![version];
    b.extend(tenant.to_le_bytes());
    b.extend(epoch.to_le_bytes());
    b.push(flags);
    b.push(entries.len() as u8);
    for (lane, next) in entries {
        b.extend(lane.to_le_bytes());
        b.extend(next.to_le_bytes());
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn epoch_of(t: &str) -> u64 {
    u64::from_le_bytes(decode(t)[17..25].try_into().unwrap())
}

#[tokio::test]
async fn a_session_reads_its_own_durable_write_through_another_process() {
    let w = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    let t = write(a, "x", true, None).await;
    let n = decode(&t);
    assert_eq!(n[26], 1, "one entry: {n:?}");
    assert_eq!(&n[27..35], &1u64.to_le_bytes(), "lane 1");
    assert_eq!(&n[35..43], &1u64.to_le_bytes(), "below sequence 1");

    let (s, h, body) = query(b, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "not_folded");
    assert!(h.contains_key("retry-after"), "{h:?}");
    // Eventual serves it stale, as it always did: to B the index does not exist yet.
    let (s, _, body) = query(b, &q("eventual"), Some(&t)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "index_not_found");

    // The refusal asked for a fold, and nothing else makes this tenant due.
    assert_eq!(b.fold_due(&requested_only()).await.folded, 1);
    let (s, h, body) = query(b, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), ["x"]);
    assert_eq!(body["meta"]["consistency"], "session");
    assert_eq!(body["meta"]["session"], json!(h[HEADER].to_str().unwrap()));
}

#[tokio::test]
async fn a_session_reads_its_own_write_on_its_own_process_without_a_fold() {
    let w = apis(&[1]);
    let t = write(&w[0], "x", true, None).await;
    let (s, _, body) = query(&w[0], &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), ["x"]);
}

#[tokio::test]
async fn a_session_never_goes_backwards_and_its_token_shrinks() {
    let w = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    let t = write(a, "x", true, None).await;
    a.fold_due(&FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::ZERO,
        bytes: 1 << 20,
    })
    .await;
    let (s, h, body) = query(b, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let back = h[HEADER].to_str().unwrap();
    assert!(epoch_of(back) >= epoch_of(&t));
    assert_eq!(epoch_of(back), body["meta"]["epoch"].as_u64().unwrap());
    // Folded: the entry is gone, and the token is its header alone.
    assert_eq!(decode(back).len(), 27, "{:?}", decode(back));

    // A token ahead of the store cannot have come from it.
    let ahead = token(1, TENANT, 1000, 0, &[]);
    let (s, _, body) = query(b, &q("session"), Some(&ahead)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_session");
}

#[tokio::test]
async fn a_covered_session_read_costs_what_eventual_does() {
    let w = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    let t = write(a, "x", true, None).await;
    b.fold_due(&FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::ZERO,
        bytes: 1 << 20,
    })
    .await;
    a.fold_due(&FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::ZERO,
        bytes: 1 << 20,
    })
    .await;
    let (s, _, eventual) = query(b, &q("eventual"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, session) = query(b, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{session}");
    assert_eq!(session["meta"]["cost"], eventual["meta"]["cost"]);
    assert_eq!(ids(&session), ["x"]);
}

#[tokio::test]
async fn a_session_across_seventeen_lanes_overflows_into_strong() {
    let lanes: Vec<u64> = (1..=18).collect();
    let w = apis(&lanes);
    let mut t: Option<String> = None;
    for (i, api) in w[..17].iter().enumerate() {
        t = Some(write(api, &format!("d{i:02}"), true, t.as_deref()).await);
    }
    let t = t.unwrap();
    let n = decode(&t);
    assert_eq!(n[25] & 1, 1, "overflow set");
    assert_eq!(n[26], 0, "and every entry dropped");
    assert_eq!(n.len(), 27);

    let reader = &w[17];
    let (s, _, body) = query(reader, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(reader.fold_due(&requested_only()).await.folded, 1);
    let (s, h, body) = query(reader, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(ids(&body).len(), 17);
    let back = decode(h[HEADER].to_str().unwrap());
    assert_eq!(back[25] & 1, 0, "overflow cleared");
    assert_eq!(back[26], 0, "the reader holds nothing unfolded");
}

#[tokio::test]
async fn a_batched_write_is_not_required_of_another_process() {
    let w = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    let t = write(a, "x", false, None).await;
    assert_eq!(decode(&t)[26], 0, "a batched write mints no entry");
    let (s, _, body) = query(a, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ids(&body), ["x"]);
    // Not refused on its account: B answers as it would with no token -- no such index yet.
    let (s, _, body) = query(b, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "index_not_found");
}

#[tokio::test]
async fn a_bad_session_is_refused() {
    let w = apis(&[1]);
    let good = write(&w[0], "x", true, None).await;
    let mut short = decode(&good);
    short.pop();
    let seventeen: Vec<(u64, u64)> = (0..17).map(|l| (l, 1)).collect();
    for bad in [
        "not base64!".to_owned(),
        token(2, TENANT, 0, 0, &[]),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(short),
        token(1, TENANT + 1, 0, 0, &[]),
        token(1, TENANT, 0, 0, &seventeen),
        token(1, TENANT, 1000, 0, &[]),
    ] {
        let (s, _, body) = query(&w[0], &q("session"), Some(&bad)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["error"]["code"], "bad_session", "{bad}");
    }
    let mut past = q("session");
    past["as_of"] = json!(1);
    let (s, _, body) = query(&w[0], &past, Some(&good)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    let (s, _, body) = query(&w[0], &q("Session"), Some(&good)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_multi_query_checks_its_session_subqueries() {
    let w = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    // The index exists for B first, so the eventual sub-query answers rather than 404s.
    write(a, "w", true, None).await;
    a.fold_due(&FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::ZERO,
        bytes: 1 << 20,
    })
    .await;
    let t = write(a, "x", true, None).await;
    // `session` second, so a merge that keeps only the first sub-query's token is caught.
    let multi = json!({"queries": [q("eventual"), q("session")]});
    let (s, _, body) = query(b, &multi, Some(&t)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    b.fold_due(&requested_only()).await;
    let (s, h, body) = query(b, &multi, Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(
        body["results"][1]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "x"),
        "{body}"
    );
    let merged = h[HEADER].to_str().unwrap();
    assert_eq!(body["meta"]["session"], json!(merged));
    assert_eq!(decode(merged).len(), 27);
    let newest = body["meta"]["epochs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_u64().unwrap())
        .max()
        .unwrap();
    assert_eq!(epoch_of(merged), newest, "the merge keeps the newest epoch");
}

/// A store whose bundle PUTs take a while, so concurrent durable writes queue behind one flush
/// and later ones find their rows already written by it.
#[derive(Debug, Default, Clone)]
struct Slow(MemoryStore);

#[async_trait::async_trait]
impl BlobStore for Slow {
    fn capabilities(&self) -> &Capabilities {
        self.0.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.0.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.0.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.0.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.0.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.0.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.0.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        if key.as_str().ends_with(".bundle") {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        self.0.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.0.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.0.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.0.list_unrestricted(prefix).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_of_many_concurrent_durable_writes_is_required_of_another_process() {
    let store = Accounted::new(Slow::default());
    let a = Api::new(store.clone(), LaneId(1)).unwrap();
    let b = Api::new(store, LaneId(2)).unwrap();
    let writes = (0..12).map(|i| {
        let a = Arc::clone(&a);
        tokio::spawn(async move {
            let body = json!({"durability": "durable",
                "documents": [{"id": format!("d{i:02}"), "vector": [1.0, 0.5]}]});
            let res = a
                .router()
                .oneshot(request(
                    TENANT,
                    "PUT",
                    "/v1/indexes/docs/documents",
                    &body,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            (
                format!("d{i:02}"),
                res.headers()[HEADER].to_str().unwrap().to_owned(),
            )
        })
    });
    let tokens: Vec<(String, String)> = futures_util::future::join_all(writes)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    for (id, t) in &tokens {
        let (s, _, body) = send_any(
            &b,
            request(
                TENANT,
                "POST",
                "/v1/indexes/docs/query",
                &q("session"),
                Some(t),
            ),
        )
        .await;
        assert_eq!(
            s,
            StatusCode::SERVICE_UNAVAILABLE,
            "{id}'s token read through B: {body}"
        );
    }
    b.fold_due(&requested_only()).await;
    for (id, t) in &tokens {
        let (s, _, body) = send_any(
            &b,
            request(
                TENANT,
                "POST",
                "/v1/indexes/docs/query",
                &q("session"),
                Some(t),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(
            body["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == json!(id))
        );
    }
}

async fn send_any<S: BlobStore>(
    api: &Arc<Api<S>>,
    req: Request<Body>,
) -> (StatusCode, HeaderMap, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_token_is_not_trusted_by_a_restarted_process_on_its_lane() {
    let store = Accounted::new(MemoryStore::new());
    let before = Api::new(store.clone(), LaneId(1)).unwrap();
    let t = write(&before, "x", true, None).await;
    drop(before);
    let after = Api::new(store, LaneId(1)).unwrap();
    let (s, _, body) = query(&after, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "not resumed: {body}");
    // Resumed past the token: the predecessor's bundle is in neither HEAD nor memory.
    write(&after, "y", true, None).await;
    let (s, _, body) = query(&after, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "resumed: {body}");
    after.fold_due(&requested_only()).await;
    let (s, _, body) = query(&after, &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), ["x", "y"]);
}

#[tokio::test]
async fn clearing_overflow_keeps_the_readers_own_unfolded_writes() {
    let lanes: Vec<u64> = (1..=18).collect();
    let w = apis(&lanes);
    let mut t: Option<String> = None;
    for (i, api) in w[..17].iter().enumerate() {
        t = Some(write(api, &format!("d{i:02}"), true, t.as_deref()).await);
    }
    // A refused strong read requests the fold that folds every lane.
    let (s, _, _) = query(&w[0], &q("strong"), None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "nothing is folded yet");
    w[0].fold_due(&requested_only()).await;
    // Lane 1 now holds a durable write only its own process has in memory.
    let t = write(&w[0], "z", true, t.as_deref()).await;
    assert_eq!(decode(&t)[25] & 1, 1, "still overflowed");
    let (s, h, body) = query(&w[0], &q("session"), Some(&t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let back = h[HEADER].to_str().unwrap().to_owned();
    let raw = decode(&back);
    assert_eq!(raw[25] & 1, 0, "overflow cleared");
    assert_eq!(raw[26], 1, "and lane 1 named: {raw:?}");
    // So another process must still wait for `z`.
    let reader = &w[17];
    let (s, _, body) = query(reader, &q("session"), Some(&back)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    reader.fold_due(&requested_only()).await;
    let (s, _, body) = query(reader, &q("session"), Some(&back)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(ids(&body).contains(&"z".to_owned()));
}
