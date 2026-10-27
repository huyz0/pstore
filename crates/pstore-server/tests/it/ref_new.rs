//! `$ref_new` in write conditions (M133): a condition may compare the current row with the
//! operation's own new values, resolved at the door once per operation.
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
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

/// A server at `PSTORE_APPLIED_WRITES=1` on lane `lane` of `store`.
fn api(store: &Accounted<MemoryStore>, lane: u64) -> A {
    let api = Api::unsecured(store.clone(), LaneId(lane)).unwrap();
    api.set_applied_writes(true);
    api
}

async fn send(api: &A, method: &str, path: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "33")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn doc(id: &str, attrs: Value) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs})
}

/// An applied write of `body`'s fields; answers the `applied` outcome.
async fn applied(api: &A, mut body: Value) -> Value {
    body["durability"] = json!("applied");
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["applied"].clone()
}

/// The outcome's `(upserted, patched, skipped)`.
fn counts(o: &Value) -> (u64, u64, u64) {
    let n = |k: &str| o[k].as_u64().unwrap();
    (n("upserted"), n("patched"), n("skipped"))
}

/// Every row of `docs`, as `id -> attributes`.
async fn rows(api: &A) -> BTreeMap<String, Map<String, Value>> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 1000, "consistency": "strong",
                   "include_attributes": true});
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let attrs = r["attributes"].as_object().cloned().unwrap_or_default();
            (r["id"].as_str().unwrap().to_owned(), attrs)
        })
        .collect()
}

/// `id`'s attribute `a`, or `Null` when the row lacks it.
async fn attr(api: &A, id: &str, a: &str) -> Value {
    rows(api).await[id].get(a).cloned().unwrap_or(Value::Null)
}

fn newer() -> Value {
    json!(["v", "Lt", {"$ref_new": "v"}])
}

/// Criterion 1: an upsert under "newer wins" applies when its `v` is larger.
#[tokio::test]
async fn newer_wins_for_an_upsert() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    applied(&a, json!({"documents": [doc("x", json!({"v": 5}))]})).await;
    let o = applied(
        &a,
        json!({"documents": [doc("x", json!({"v": 7}))], "upsert_condition": newer()}),
    )
    .await;
    assert_eq!(counts(&o), (1, 0, 0), "{o}");
    assert_eq!(attr(&a, "x", "v").await, 7);
    let o = applied(
        &a,
        json!({"documents": [doc("x", json!({"v": 3}))], "upsert_condition": newer()}),
    )
    .await;
    assert_eq!(counts(&o), (0, 0, 1), "{o}");
    assert_eq!(attr(&a, "x", "v").await, 7);
}

/// Criterion 2: a patch's reference is the value it sets; one it does not set is absent.
#[tokio::test]
async fn a_patch_references_what_it_sets() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    applied(
        &a,
        json!({"documents": [doc("x", json!({"v": 5})), doc("y", json!({"w": 0}))]}),
    )
    .await;
    let patch = |attrs: Value, cond: Value| json!({"patch_rows": [{"id": "x", "attributes": attrs}], "patch_condition": cond});
    let o = applied(&a, patch(json!({"v": 9}), newer())).await;
    assert_eq!(counts(&o), (0, 1, 0), "{o}");
    let o = applied(&a, patch(json!({"v": 2}), newer())).await;
    assert_eq!(counts(&o), (0, 0, 1), "{o}");
    // Not setting `v`: `Lt` against an absent reference is false.
    let o = applied(&a, patch(json!({"w": 1}), newer())).await;
    assert_eq!(counts(&o), (0, 0, 1), "{o}");
    assert_eq!(attr(&a, "x", "v").await, 9);
    assert_eq!(attr(&a, "x", "w").await, Value::Null);
    // Unsetting `v`: the reference is absent, so `NotEq` means "the row has `v`".
    let unset = |id: &str| {
        json!({"patch_rows": [{"id": id, "attributes": {"v": null, "w": 1}}],
               "patch_condition": ["v", "NotEq", {"$ref_new": "v"}]})
    };
    let o = applied(&a, unset("y")).await;
    assert_eq!(counts(&o), (0, 0, 1), "{o}");
    assert_eq!(attr(&a, "y", "w").await, 0);
    let o = applied(&a, unset("x")).await;
    assert_eq!(counts(&o), (0, 1, 0), "{o}");
    assert_eq!(attr(&a, "x", "v").await, Value::Null);
    assert_eq!(attr(&a, "x", "w").await, 1);
}

/// Criterion 3: an absent reference, against rows with and without the attribute.
#[tokio::test]
async fn absent_references() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    applied(
        &a,
        json!({"documents": [doc("has", json!({"v": 1, "n": 0})), doc("lacks", json!({"n": 0}))]}),
    )
    .await;
    // Every upsert lacks `v` and sets `n` to 1: the outcome says which applied.
    let upsert = |id: &str, op: &str| {
        json!({"documents": [doc(id, json!({"n": 1}))],
               "upsert_condition": ["v", op, {"$ref_new": "v"}]})
    };
    for (op, has, lacks) in [
        ("Eq", 0, 1),
        ("NotEq", 1, 0),
        ("Lt", 0, 0),
        ("Lte", 0, 0),
        ("Gt", 0, 0),
        ("Gte", 0, 0),
    ] {
        for (id, want) in [("has", has), ("lacks", lacks)] {
            let o = applied(&a, upsert(id, op)).await;
            assert_eq!(o["upserted"], want, "{op} over {id}: {o}");
        }
        // Put both rows back.
        applied(
            &a,
            json!({"documents": [doc("has", json!({"v": 1, "n": 0})),
                                 doc("lacks", json!({"n": 0}))]}),
        )
        .await;
    }
    // A missing id applies, whatever the condition (M13).
    let o = applied(&a, upsert("new", "Gt")).await;
    assert_eq!(counts(&o), (1, 0, 0), "{o}");
}

/// Criterion 4: `id` resolves to the operation's id.
#[tokio::test]
async fn an_id_reference_is_the_operations_id() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    applied(
        &a,
        json!({"documents": [doc("x", json!({"owner": "x"})), doc("y", json!({"owner": "x"}))]}),
    )
    .await;
    let o = applied(
        &a,
        json!({"documents": [doc("x", json!({"owner": "z"})), doc("y", json!({"owner": "z"}))],
               "upsert_condition": ["owner", "Eq", {"$ref_new": "id"}]}),
    )
    .await;
    assert_eq!(counts(&o), (1, 0, 1), "{o}");
    assert_eq!(attr(&a, "x", "owner").await, "z");
    assert_eq!(attr(&a, "y", "owner").await, "x");
}

/// Criterion 5: one id twice in one request, each against the version the one before left.
#[tokio::test]
async fn repeated_ids_resolve_in_order() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    for (first, second, upserted) in [(2, 1, 1), (1, 2, 2)] {
        applied(&a, json!({"documents": [doc("x", json!({"v": 0}))]})).await;
        let o = applied(
            &a,
            json!({"documents": [doc("x", json!({"v": first})), doc("x", json!({"v": second}))],
                   "upsert_condition": newer()}),
        )
        .await;
        assert_eq!(
            counts(&o),
            (upserted, 0, 2 - upserted),
            "{first} then {second}: {o}"
        );
        assert_eq!(attr(&a, "x", "v").await, 2, "{first} then {second}");
    }
}

/// Criterion 6: 100 applied writers over 4 servers, then a fresh server reads what they did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adversarial_writers_never_both_win() {
    let store = Accounted::new(MemoryStore::new());
    let servers: Vec<A> = (1..=4).map(|l| api(&store, l)).collect();
    let writes = (0..100i64).map(|i| {
        let a = Arc::clone(&servers[(i % 4) as usize]);
        async move {
            let claim = applied(
                &a,
                json!({"documents": [doc("lock", json!({"owner": i}))],
                       "upsert_condition": ["owner", "Eq", null]}),
            )
            .await;
            let newer = applied(
                &a,
                json!({"documents": [doc("x", json!({"v": i}))], "upsert_condition": newer()}),
            )
            .await;
            (i, claim, newer)
        }
    });
    let all = futures_util::future::join_all(writes.map(tokio::spawn)).await;
    let all: Vec<_> = all.into_iter().map(Result::unwrap).collect();
    let winners: Vec<i64> = all
        .iter()
        .filter(|(_, c, _)| c["upserted"] == 1)
        .map(|(i, _, _)| *i)
        .collect();
    assert_eq!(winners.len(), 1, "claims won: {winners:?}");
    for (i, claim, newer) in &all {
        assert_eq!(counts(claim).0 + counts(claim).2, 1, "{i}: {claim}");
        assert_eq!(counts(newer).0 + counts(newer).2, 1, "{i}: {newer}");
    }
    assert_eq!(all[99].2["upserted"], 1, "{}", all[99].2);
    // Restart: a server that has seen none of it reads both rows as the responses imply.
    let fresh = api(&store, 9);
    assert_eq!(attr(&fresh, "lock", "owner").await, winners[0]);
    assert_eq!(attr(&fresh, "x", "v").await, 99);
}

/// Criterion 7: what is refused, before anything is buffered.
#[tokio::test]
async fn ref_new_refusals() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    let r = |name: &str| json!({"$ref_new": name});
    let docs = json!([doc("x", json!({"v": 1, "tags": ["a"]}))]);
    let refused = [
        json!({"deletes": ["x"], "delete_condition": ["v", "Eq", r("v")]}),
        json!({"documents": docs, "upsert_condition": ["v", "In", [r("v")]]}),
        json!({"documents": docs, "upsert_condition": ["v", "In", r("v")]}),
        json!({"documents": docs, "upsert_condition": ["v", "Contains", r("v")]}),
        json!({"documents": docs, "upsert_condition": ["v", "Glob", r("v")]}),
        json!({"documents": docs, "upsert_condition": ["v", "Eq", {"$ref_new": 1}]}),
        json!({"documents": docs, "upsert_condition": ["v", "Eq", r("$op")]}),
        json!({"documents": docs, "upsert_condition": ["v", "Eq", r("")]}),
        json!({"documents": docs, "upsert_condition": ["v", "Eq", {"$ref_new": "v", "x": 1}]}),
        json!({"documents": docs, "patch_by_filter": {"filters": ["v", "Eq", r("v")],
                                                      "attributes": {"v": 2}}}),
    ];
    for body in &refused {
        let mut body = body.clone();
        body["durability"] = json!("applied");
        let (s, b) = send(&a, "PUT", "/v1/indexes/docs/documents", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
    }
    // New refusals, with their own text.
    let new = [
        (
            json!({"documents": docs, "upsert_condition":
                    ["And", [["v", "Lt", r("v")], ["s", "Glob", "a*"]]]}),
            "cannot sit beside",
        ),
        (
            json!({"documents": docs, "upsert_condition": ["tags", "Eq", r("tags")]}),
            "is an array",
        ),
    ];
    for (body, text) in &new {
        let mut body = body.clone();
        body["durability"] = json!("applied");
        let (s, b) = send(&a, "PUT", "/v1/indexes/docs/documents", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(text), "{msg}");
    }
    // Nothing was buffered: the index was never made.
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "consistency": "strong"});
    let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    // And a query filter refuses a reference, over an index that exists.
    applied(&a, json!({"documents": docs})).await;
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "consistency": "strong",
                   "filters": ["v", "Eq", r("v")]});
    let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

/// References under `And`, `Or` and `Not`, beside constant clauses, with every operator
/// against a present value.
#[tokio::test]
async fn references_nest_and_compare() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    let reset = json!({"documents": [doc("x", json!({"v": 5, "k": "a"}))]});
    let r = json!({"$ref_new": "v"});
    // Each upsert writes k = "b" and a `v` over v = 5: 5 itself, then unequal values.
    let cases = [
        (json!(["v", "Eq", r]), 1),
        (json!(["v", "NotEq", r]), 0),
        (json!(["v", "Lt", r]), 0),
        (json!(["v", "Lte", r]), 1),
        (json!(["v", "Gt", r]), 0),
        (json!(["v", "Gte", r]), 1),
        (json!(["Not", ["v", "Lt", r]]), 1),
        (json!(["Or", [["v", "Lt", r], ["k", "Eq", "a"]]]), 1),
        (json!(["Or", [["v", "Lt", r], ["k", "Eq", "z"]]]), 0),
        (
            json!(["And", [["v", "Gte", r], ["Not", ["k", "Eq", "z"]]]]),
            1,
        ),
    ];
    let unequal = [
        (json!(["v", "Gt", r]), 3, 1),
        (json!(["v", "Gt", r]), 7, 0),
        (json!(["v", "Gte", r]), 3, 1),
        (json!(["v", "Gte", r]), 7, 0),
        (json!(["v", "Lt", r]), 3, 0),
        (json!(["v", "Lte", r]), 7, 1),
    ];
    let cases = cases.into_iter().map(|(c, w)| (c, 5, w)).chain(unequal);
    for (cond, v, want) in cases {
        applied(&a, reset.clone()).await;
        let o = applied(
            &a,
            json!({"documents": [doc("x", json!({"v": v, "k": "b"}))], "upsert_condition": cond}),
        )
        .await;
        assert_eq!(o["upserted"], want, "{cond} writing {v}: {o}");
    }
}

/// Malformed templates, and costly clauses anywhere beside a reference, are refused.
#[tokio::test]
async fn malformed_templates_are_refused() {
    let a = api(&Accounted::new(MemoryStore::new()), 1);
    let r = json!({"$ref_new": "v"});
    let mut deep = json!(["v", "Lt", r]);
    for _ in 0..80 {
        deep = json!(["Not", deep]);
    }
    let refused = [
        (json!(["Or", r]), "array of filters"),
        (json!([1, "Lt", r]), "naming no reserved attribute"),
        (json!(["$id", "Lt", r]), "naming no reserved attribute"),
        (
            json!(["Or", [["v", "Lt", r], ["Not", ["s", "Fuzzy", {"value": "ab", "max_edits": 1}]]]]),
            "cannot sit beside",
        ),
        // M145: the vendor's `Fuzzy` is as costly as M15's.
        (
            json!(["And", [["v", "Lt", r], ["s", "Fuzzy", "abcd",
                   {"max_edit_distance": [{"min_query_chars": 3, "distance": 0}]}]]]),
            "cannot sit beside",
        ),
        (
            json!([
                "Not",
                [
                    "And",
                    [["v", "Lt", r], ["Or", [["s", "ContainsAllTokens", "a b"]]]]
                ]
            ]),
            "cannot sit beside",
        ),
        (
            json!(["And", [["v", "Lt", r], ["s", "NotGlob", "a*"]]]),
            "cannot sit beside",
        ),
        (deep, "nests too deeply"),
    ];
    for (cond, text) in refused {
        let body = json!({"durability": "applied", "upsert_condition": cond,
                          "documents": [doc("x", json!({"v": 1}))]});
        let (s, b) = send(&a, "PUT", "/v1/indexes/docs/documents", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(text), "{text}: {msg}");
    }
}
