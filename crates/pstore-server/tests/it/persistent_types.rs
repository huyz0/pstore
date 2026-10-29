//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Persistent attribute types (M173): a write's declaration is recorded in its index's
//! schema at the fold and holds for every later write, in this process and in any other.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

async fn send(api: &A, method: &str, uri: &str, body: Option<&Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "23")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// A durable write of `docs` to `index`, declaring `schema` when it is not null.
async fn write(api: &A, index: &str, docs: Value, schema: Value) -> (StatusCode, Value) {
    let mut body = json!({"durability": "durable", "documents": docs});
    if !schema.is_null() {
        body["schema"] = schema;
    }
    let uri = format!("/v1/indexes/{index}/documents");
    send(api, "PUT", &uri, Some(&body)).await
}

async fn wrote(api: &A, index: &str, docs: Value, schema: Value) {
    let (s, b) = write(api, index, docs, schema).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Every row of `index` by id, with its attributes.
async fn rows(api: &A, index: &str) -> Value {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 100, "include_attributes": true});
    let uri = format!("/v1/indexes/{index}/query");
    let (s, b) = send(api, "POST", &uri, Some(&q)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"].clone()
}

fn ids(results: &Value) -> Vec<&str> {
    results
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect()
}

async fn summary(api: &A, index: &str) -> Value {
    let (s, b) = send(api, "GET", &format!("/v1/indexes/{index}"), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn api(store: &Accounted<MemoryStore>, lane: u64) -> A {
    Api::unsecured(store.clone(), LaneId(lane)).unwrap()
}

const U1: &str = "00000000-0000-4000-8000-000000000001";
const U2: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c8d7e6f5a4b";

fn row(id: &str, attrs: Value) -> Value {
    json!({"id": id, "vector": [1.0, 0.0], "attributes": attrs})
}

fn four() -> Value {
    json!({"owner": "uuid", "seq": "uint64", "at": "datetime", "blob": "bytes"})
}

#[tokio::test]
async fn a_recorded_type_reads_every_later_write_after_a_restart() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1);
    let first = row(
        "a",
        json!({"owner": U1, "seq": 1, "at": "2026-10-28T00:00:00Z", "blob": "AAE="}),
    );
    wrote(&a, "n", json!([first]), four()).await;
    fold(&a).await;
    drop(a);

    // A restarted API, declaring nothing: each value is read as its recorded kind.
    let b = api(&store, 1);
    let second = row(
        "b",
        json!({
            "owner": U2.to_uppercase(),
            "seq": u64::MAX,
            "at": "2026-10-28T01:00:00+01:00",
            "blob": "/w==",
        }),
    );
    wrote(&b, "n", json!([second]), Value::Null).await;
    let got = rows(&b, "n").await;
    let attrs = &got[1]["attributes"];
    assert_eq!(attrs["owner"], U2, "a uuid, answered lowercase: {got}");
    assert_eq!(attrs["seq"], json!(u64::MAX), "exactly: {got}");
    assert_eq!(attrs["at"], "2026-10-28T00:00:00Z", "formatted: {got}");
    assert_eq!(attrs["blob"], "/w==", "{got}");
    fold(&b).await;
    assert_eq!(rows(&b, "n").await, got, "the same after a fold");

    // A patch's attributes too, and a by-filter patch's, from a process whose first write
    // it is.
    let c = api(&store, 1);
    let body = json!({
        "durability": "durable",
        "patch_rows": [{"id": "a", "attributes": {"owner": U2.to_uppercase()}}],
        "patch_by_filter": {"filters": ["id", "Eq", "b"], "attributes": {"seq": u64::MAX - 1}},
    });
    let (s, r) = send(&c, "PUT", "/v1/indexes/n/documents", Some(&body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    fold(&c).await;
    let patched = rows(&c, "n").await;
    assert_eq!(patched[0]["attributes"]["owner"], U2, "{patched}");
    assert_eq!(
        patched[1]["attributes"]["seq"],
        json!(u64::MAX - 1),
        "{patched}"
    );
    drop(c);

    // A malformed one is refused, and nothing of its write is buffered.
    for (name, bad) in [
        ("owner", json!("not-a-uuid")),
        ("seq", json!(-1)),
        ("at", json!("yesterday")),
        ("blob", json!("AAE")),
    ] {
        let docs = json!([row("c", json!({"seq": 2})), row("d", json!({name: bad}))]);
        let (s, body) = write(&b, "n", docs, Value::Null).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name}: {body}");
        assert!(
            body["error"]["message"].as_str().unwrap().contains(name),
            "{body}"
        );
        assert_eq!(ids(&rows(&b, "n").await), ["a", "b"], "{name}");
    }
}

#[tokio::test]
async fn an_unfolded_declaration_reads_the_processs_next_write() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1);
    // `n` has a recorded schema, with no types; `fresh` has none at all.
    wrote(&a, "n", json!([row("z", json!({"k": 1}))]), Value::Null).await;
    fold(&a).await;
    for index in ["n", "fresh"] {
        wrote(
            &a,
            index,
            json!([row("a", json!({"owner": U1}))]),
            json!({"owner": "uuid"}),
        )
        .await;
        wrote(
            &a,
            index,
            json!([row("b", json!({"owner": U2.to_uppercase()}))]),
            Value::Null,
        )
        .await;
        let got = rows(&a, index).await;
        let b = got.as_array().unwrap().iter().find(|r| r["id"] == "b");
        assert_eq!(b.unwrap()["attributes"]["owner"], U2, "{index}: {got}");
        // Read as a uuid, so a string that is not one is refused.
        let (s, body) = write(
            &a,
            index,
            json!([row("c", json!({"owner": "x"}))]),
            Value::Null,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{index}: {body}");
    }
}

/// Refuses `owner` declared bytes, naming the attribute and both kinds; nothing buffered.
async fn refused_as_bytes(a: &A, index: &str) {
    let docs = json!([row("q", json!({"owner": "AAE="}))]);
    let (s, body) = write(a, index, docs, json!({"owner": "bytes"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "schema_conflict", "{body}");
    let m = body["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("`owner` as uuid") && m.contains("`owner` as bytes"),
        "{m}"
    );
    assert!(!ids(&rows(a, index).await).contains(&"q"), "{index}");
}

#[tokio::test]
async fn a_declaration_differing_from_a_known_type_is_refused() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1);
    wrote(
        &a,
        "n",
        json!([row("a", json!({"owner": U1}))]),
        json!({"owner": "uuid"}),
    )
    .await;
    // Unfolded: this process's own declaration.
    refused_as_bytes(&a, "n").await;
    fold(&a).await;
    // Recorded, and read by a process that has never written.
    refused_as_bytes(&a, "n").await;
    refused_as_bytes(&api(&store, 2), "n").await;
    // The same kind again is no conflict.
    wrote(
        &a,
        "n",
        json!([row("b", json!({"owner": U2}))]),
        json!({"owner": "uuid"}),
    )
    .await;
}

#[tokio::test]
async fn concurrent_first_declarations_record_the_first_in_lane_order() {
    let store = Accounted::new(MemoryStore::new());
    let (one, two) = (api(&store, 1), api(&store, 2));
    // Lane 2 writes first in time; lane 1 is first in lane order.
    wrote(
        &two,
        "n",
        json!([row("b", json!({"owner": "AAE="}))]),
        json!({"owner": "bytes"}),
    )
    .await;
    wrote(
        &one,
        "n",
        json!([row("a", json!({"owner": U1}))]),
        json!({"owner": "uuid"}),
    )
    .await;
    fold(&one).await;
    let meta = summary(&one, "n").await;
    assert_eq!(
        meta["schema"]["attributes"],
        json!({"owner": "uuid"}),
        "{meta}"
    );
    assert_eq!(meta["quarantined_rows"], 1, "{meta}");
    assert_eq!(ids(&rows(&one, "n").await), ["a"]);

    // The losing process reads HEAD, and its own declaration gives way to the recorded one.
    assert_eq!(ids(&rows(&two, "n").await), ["a"]);
    wrote(
        &two,
        "n",
        json!([row("c", json!({"owner": U2.to_uppercase()}))]),
        Value::Null,
    )
    .await;
    fold(&two).await;
    let got = rows(&one, "n").await;
    assert_eq!(ids(&got), ["a", "c"], "{got}");
    assert_eq!(got[1]["attributes"]["owner"], U2, "{got}");
    assert_eq!(summary(&one, "n").await["quarantined_rows"], 1);
}

#[tokio::test]
async fn an_untyped_row_meeting_a_declaration_is_quarantined_and_patches_are_not() {
    let store = Accounted::new(MemoryStore::new());
    let (one, two) = (api(&store, 1), api(&store, 2));
    // A row sealed before `owner` has a type, holding a plain string.
    wrote(
        &one,
        "n",
        json!([row("old", json!({"owner": "plain"}))]),
        Value::Null,
    )
    .await;
    fold(&one).await;
    // Lane 1 writes `owner` as a plain string, lane 2 declares it a uuid, no fold between;
    // the string row comes first in lane order and is quarantined all the same.
    wrote(
        &one,
        "n",
        json!([row("s", json!({"owner": "plain"}))]),
        Value::Null,
    )
    .await;
    wrote(
        &two,
        "n",
        json!([row("u", json!({"owner": U1}))]),
        json!({"owner": "uuid"}),
    )
    .await;
    fold(&one).await;
    let meta = summary(&one, "n").await;
    assert_eq!(
        meta["schema"]["attributes"],
        json!({"owner": "uuid"}),
        "{meta}"
    );
    assert_eq!(meta["quarantined_rows"], 1, "{meta}");
    assert_eq!(ids(&rows(&one, "n").await), ["old", "u"]);

    // A patch of another attribute onto the sealed row holding an untyped `owner` applies.
    let body = json!({
        "durability": "durable",
        "patch_rows": [{"id": "old", "attributes": {"note": "kept"}}],
    });
    let (s, b) = send(&one, "PUT", "/v1/indexes/n/documents", Some(&body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&one).await;
    let got = rows(&one, "n").await;
    assert_eq!(got[0]["attributes"]["note"], "kept", "{got}");
    assert_eq!(got[0]["attributes"]["owner"], "plain", "{got}");
    assert_eq!(summary(&one, "n").await["quarantined_rows"], 1);
}

#[tokio::test]
async fn the_summary_answers_the_recorded_types() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1);
    wrote(&a, "plain", json!([row("a", json!({"k": 1}))]), Value::Null).await;
    let first = row(
        "a",
        json!({"owner": U1, "seq": 1, "at": "2026-10-28T00:00:00Z", "blob": "AAE="}),
    );
    wrote(&a, "n", json!([first]), four()).await;
    fold(&a).await;
    let want = json!({"owner": "uuid", "seq": "uint64", "at": "datetime", "blob": "bytes"});
    assert_eq!(summary(&a, "n").await["schema"]["attributes"], want);
    assert_eq!(
        summary(&a, "plain").await["schema"]["attributes"],
        json!({})
    );
    drop(a);
    let a = api(&store, 1);
    assert_eq!(summary(&a, "n").await["schema"]["attributes"], want);
    assert_eq!(
        summary(&a, "plain").await["schema"]["attributes"],
        json!({})
    );

    // Re-created after a drop, the index records nothing: `owner` is a string again.
    let (s, b) = send(&a, "DELETE", "/v1/indexes/n", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&a).await;
    wrote(
        &a,
        "n",
        json!([row("b", json!({"owner": "plain"}))]),
        Value::Null,
    )
    .await;
    fold(&a).await;
    let meta = summary(&a, "n").await;
    assert_eq!(meta["schema"]["attributes"], json!({}), "{meta}");
    assert_eq!(rows(&a, "n").await[0]["attributes"]["owner"], "plain");
}

#[tokio::test]
async fn a_dropped_indexs_type_reads_no_later_write() {
    // Dropped by another process: this one's recorded type is stale, and a refusal by it
    // re-reads HEAD before it stands.
    let store = Accounted::new(MemoryStore::new());
    let (one, two) = (api(&store, 1), api(&store, 2));
    wrote(
        &one,
        "n",
        json!([row("a", json!({"owner": U1}))]),
        json!({"owner": "uuid"}),
    )
    .await;
    fold(&one).await;
    let (s, b) = send(&two, "DELETE", "/v1/indexes/n", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&two).await;
    for _ in 0..2 {
        wrote(
            &one,
            "n",
            json!([row("p", json!({"owner": "plain"}))]),
            Value::Null,
        )
        .await;
    }
    fold(&one).await;
    assert_eq!(summary(&two, "n").await["schema"]["attributes"], json!({}));
    assert_eq!(rows(&one, "n").await[0]["attributes"]["owner"], "plain");

    // Dropped by this process, its own unfolded declaration with it.
    let store = Accounted::new(MemoryStore::new());
    let one = api(&store, 1);
    wrote(
        &one,
        "m",
        json!([row("a", json!({"owner": U1}))]),
        json!({"owner": "uuid"}),
    )
    .await;
    let (s, b) = send(&one, "DELETE", "/v1/indexes/m", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&one).await;
    wrote(
        &one,
        "m",
        json!([row("p", json!({"owner": "plain"}))]),
        Value::Null,
    )
    .await;
    fold(&one).await;
    let meta = summary(&one, "m").await;
    assert_eq!(meta["schema"]["attributes"], json!({}), "{meta}");
    assert_eq!(rows(&one, "m").await[0]["attributes"]["owner"], "plain");
}

#[tokio::test]
async fn a_refusal_by_the_requests_own_type_reads_nothing() {
    use pstore_blob::OpClass;
    use pstore_types::TenantId;
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1);
    // HEAD read once, and no type recorded for the index or held for it.
    wrote(&a, "plain", json!([row("a", json!({"k": 1}))]), Value::Null).await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(23), c));
    let before = spent();
    let (s, b) = write(
        &a,
        "plain",
        json!([row("b", json!({"x": "not-a-uuid"}))]),
        json!({"x": "uuid"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(
        spent(),
        before,
        "only a type the request did not declare re-reads HEAD"
    );
}

/// A restarted API whose first request is patches alone reads them by the recorded types:
/// the door reads HEAD for a write holding attribute values, whatever operation holds them.
#[tokio::test]
async fn a_patch_alone_after_a_restart_is_read_by_the_recorded_type() {
    const U3: &str = "abcdef01-2345-4678-9abc-def012345678";
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 1);
    wrote(
        &a,
        "n",
        json!([row("a", json!({"owner": U1}))]),
        json!({"owner": "uuid"}),
    )
    .await;
    fold(&a).await;
    drop(a);
    let owner = |store: &Accounted<MemoryStore>, body: Value| {
        let store = store.clone();
        async move {
            let b = api(&store, 1);
            let (s, r) = send(&b, "PUT", "/v1/indexes/n/documents", Some(&body)).await;
            assert_eq!(s, StatusCode::OK, "{body} -> {r}");
            fold(&b).await;
            rows(&b, "n").await[0]["attributes"]["owner"].clone()
        }
    };
    let patch = json!({"durability": "durable",
                       "patch_rows": [{"id": "a", "attributes": {"owner": U2.to_uppercase()}}]});
    assert_eq!(owner(&store, patch).await, U2);
    let by_filter = json!({"durability": "durable",
                           "patch_by_filter": {"filters": ["id", "Eq", "a"],
                                               "attributes": {"owner": U3.to_uppercase()}}});
    assert_eq!(owner(&store, by_filter).await, U3);
}
