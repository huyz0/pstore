//! `durability: "applied"` (M129): the response waits for the commit that folds the
//! request, and reports what the fold decided rather than what was submitted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const TENANT: TenantId = TenantId(9);

fn api(store: &Accounted<MemoryStore>, lane: u64, applied: bool) -> Arc<Api<MemoryStore>> {
    let api = Api::unsecured(store.clone(), LaneId(lane)).unwrap();
    api.set_applied_writes(applied);
    api
}

async fn send(
    api: &Arc<Api<MemoryStore>>,
    method: &str,
    path: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "9")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn write(api: &Arc<Api<MemoryStore>>, body: Value) -> Value {
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn doc(id: &str, n: i64) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": {"n": n}})
}

fn counts(b: &Value) -> [u64; 5] {
    let a = &b["applied"];
    ["upserted", "deleted", "patched", "skipped", "rejected"]
        .map(|k| a[k].as_u64().unwrap_or_else(|| panic!("no {k} in {b}")))
}

#[tokio::test]
async fn applied_counts_are_what_the_fold_decided() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1, true);
    write(
        &a,
        json!({"durability": "durable", "documents": [doc("a", 1), doc("b", 2)]}),
    )
    .await;
    // An upsert of a new id and an existing one; a delete of a present id and an absent one.
    let b = write(
        &a,
        json!({"durability": "applied", "documents": [doc("c", 3), doc("a", 10)],
               "deletes": ["b", "nope"]}),
    )
    .await;
    assert_eq!(b["completion"], "applied", "{b}");
    assert_eq!(b["operation"].as_str().unwrap().len(), 32, "{b}");
    assert_eq!(counts(&b), [2, 1, 0, 1, 0], "{b}");
    // The submitted counters keep their meaning.
    assert_eq!(
        (
            b["documents_written"].as_u64(),
            b["documents_deleted"].as_u64()
        ),
        (Some(2), Some(2))
    );
    // Conditions: a has n = 10, so the first patch applies and the second does not.
    let b = write(
        &a,
        json!({"durability": "applied",
               "patch_rows": [{"id": "a", "attributes": {"n": 11}}, {"id": "zz", "attributes": {"n": 1}}],
               "patch_condition": ["n", "Eq", 10]}),
    )
    .await;
    assert_eq!(counts(&b), [0, 0, 1, 1, 0], "{b}");
    // By filter, in rows: a (11) and c (3) are patched, then both deleted.
    let b = write(
        &a,
        json!({"durability": "applied",
               "patch_by_filter": {"filters": ["n", "Gte", 3], "attributes": {"tag": "x"}},
               "delete_by_filter": ["tag", "Eq", "x"]}),
    )
    .await;
    assert_eq!(counts(&b), [0, 2, 2, 0, 0], "{b}");
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "consistency": "strong"});
    let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let text = b.to_string();
    assert!(!text.contains("\"a\"") && !text.contains("\"c\""), "{b}");
}

#[tokio::test]
async fn strong_reads_see_an_applied_write() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1, true);
    let other = api(&store, 2, true);
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "consistency": "strong"});
    // The control: a durable write's rows are not folded yet, and another server's strong
    // read refuses rather than answering without them.
    write(
        &a,
        json!({"durability": "durable", "documents": [doc("d", 1)]}),
    )
    .await;
    let (s, b) = send(&other, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    write(
        &a,
        json!({"durability": "applied", "documents": [doc("e", 1)]}),
    )
    .await;
    let (s, b) = send(&other, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let text = b.to_string();
    assert!(text.contains("\"d\"") && text.contains("\"e\""), "{b}");
}

#[tokio::test]
async fn applied_needs_the_rollout_flag() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1, false);
    let before: Vec<u64> = [OpClass::Read, OpClass::Write, OpClass::List]
        .map(|c| store.count(TENANT, c))
        .to_vec();
    let (s, b) = send(
        &a,
        "PUT",
        "/v1/indexes/docs/documents",
        &json!({"durability": "applied", "documents": [doc("a", 1)]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "unsupported_durability", "{b}");
    let after: Vec<u64> = [OpClass::Read, OpClass::Write, OpClass::List]
        .map(|c| store.count(TENANT, c))
        .to_vec();
    assert_eq!(before, after, "a refused applied write made blob requests");
    // And a durable write's response carries none of the applied fields.
    let b = write(
        &a,
        json!({"durability": "durable", "documents": [doc("a", 1)]}),
    )
    .await;
    for k in ["completion", "operation", "applied", "outcome_unavailable"] {
        assert!(b.get(k).is_none(), "{k} in {b}");
    }
}
