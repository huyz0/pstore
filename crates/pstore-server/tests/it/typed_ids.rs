//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Typed document ids (M176): an index's ids are strings, `uint64`s or `uuid`s, held in a
//! form that orders as the values do and answered as written.

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
type S = Accounted<MemoryStore>;

/// An API on lane `lane`, answering applied writes and their affected ids.
fn api(store: &S, lane: u64) -> A {
    let a = Api::unsecured(store.clone(), LaneId(lane)).unwrap();
    a.set_applied_writes(true);
    a.set_affected_ids(true);
    a
}

async fn send(api: &A, method: &str, uri: &str, body: Option<&Value>) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "41")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A durable write of `body` to `index`.
async fn put(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    let mut body = body.clone();
    if body.get("durability").is_none() {
        body["durability"] = json!("durable");
    }
    send(
        api,
        "PUT",
        &format!("/v1/indexes/{index}/documents"),
        Some(&body),
    )
    .await
}

async fn write(api: &A, index: &str, body: Value) -> Value {
    let (s, b) = put(api, index, &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    b
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", Some(&json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &A, index: &str, q: &Value) -> Value {
    let (s, b) = send(api, "POST", &format!("/v1/indexes/{index}/query"), Some(q)).await;
    assert_eq!(s, StatusCode::OK, "{q}: {b}");
    b
}

/// The ids `index` answers, `rank_by: id` in `dir`, under `filter` when not null.
async fn ids(api: &A, index: &str, dir: &str, filter: Value) -> Vec<Value> {
    let mut q = json!({"rank_by": ["id", dir], "top_k": 100});
    if !filter.is_null() {
        q["filters"] = filter;
    }
    let b = query(api, index, &q).await;
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect()
}

/// Each row's `n`, keyed by its id as answered.
async fn ns(api: &A, index: &str) -> Vec<(Value, Value)> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 100, "include_attributes": ["n"]});
    query(api, index, &q).await["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].clone(), r["attributes"]["n"].clone()))
        .collect()
}

async fn summary(api: &A, index: &str) -> Value {
    let (s, b) = send(api, "GET", &format!("/v1/indexes/{index}"), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn doc(id: Value, n: i64) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": {"n": n}})
}

const SAFE: u64 = (1 << 53) + 1;

/// Criterion 1: `uint64` ids, answered exactly and ordered as numbers, everywhere.
#[tokio::test]
async fn uint64_ids_answer_exactly_and_order_as_numbers() {
    let store = S::new(MemoryStore::new());
    let a = api(&store, 1);
    let b = write(
        &a,
        "u",
        json!({"durability": "applied", "affected_ids": true,
               "documents": [doc(json!(10), 1), doc(json!(u64::MAX), 2)],
               "upsert_columns": {"id": [2, SAFE], "vector": [[1.0, 0.5], [1.0, 0.5]],
                                  "n": [3, 4]}}),
    )
    .await;
    assert_eq!(
        b["applied"]["ids"]["upserted"],
        json!([2, 10, SAFE, u64::MAX]),
        "{b}"
    );
    let check = |a: A| async move {
        assert_eq!(
            json!(ids(&a, "u", "asc", Value::Null).await),
            json!([2, 10, SAFE, u64::MAX])
        );
        assert_eq!(
            json!(ids(&a, "u", "desc", Value::Null).await),
            json!([u64::MAX, SAFE, 10, 2])
        );
        for (f, want) in [
            (json!(["id", "Eq", SAFE]), json!([SAFE])),
            (json!(["id", "In", [2, u64::MAX, 7]]), json!([2, u64::MAX])),
            (json!(["id", "Gt", 2]), json!([10, SAFE, u64::MAX])),
            (json!(["id", "Lt", SAFE]), json!([2, 10])),
            (json!(["id", "Eq", "10"]), json!([])),
        ] {
            assert_eq!(json!(ids(&a, "u", "asc", f.clone()).await), want, "{f}");
        }
        // Two queries ranking the rows in opposite orders tie every row: the id decides, as
        // a number.
        let q =
            |dir: &str| json!({"rank_by": ["id", dir], "top_k": 2, "filters": ["id", "Lt", SAFE]});
        let fused = query(
            &a,
            "u",
            &json!({"queries": [q("asc"), q("desc")], "rerank_by": ["RRF"]}),
        )
        .await;
        let got: Vec<&Value> = fused["results"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| &r["id"])
            .collect();
        assert_eq!(json!(got), json!([2, 10]), "{fused}");
    };
    check(Arc::clone(&a)).await;
    fold(&a).await;
    check(Arc::clone(&a)).await;
    check(api(&store, 1)).await;

    // A patch by row and by column, a delete, a delete by filter and a conditional upsert,
    // each by an integer id.
    let b = write(
        &a,
        "u",
        json!({"durability": "applied", "affected_ids": true,
               "patch_rows": [{"id": 2, "attributes": {"n": 20}}],
               "patch_columns": {"id": [10], "n": [100]},
               "deletes": [u64::MAX]}),
    )
    .await;
    assert_eq!(b["applied"]["ids"]["patched"], json!([2, 10]), "{b}");
    assert_eq!(b["applied"]["ids"]["deleted"], json!([u64::MAX]), "{b}");
    write(
        &a,
        "u",
        json!({"durability": "applied", "delete_by_filter": ["id", "Eq", SAFE]}),
    )
    .await;
    write(
        &a,
        "u",
        json!({"durability": "applied", "upsert_condition": ["id", "Eq", 10],
               "documents": [doc(json!(10), 1000), doc(json!(2), 2000)]}),
    )
    .await;
    // The condition reads the stored row: 10 exists and matches, 2 exists and does not.
    assert_eq!(
        ns(&a, "u").await,
        [(json!(2), json!(20)), (json!(10), json!(1000))]
    );
}

const UP: &str = "6F1E2D3C-4B5A-4968-8776-655443322110";
const LOW: &str = "6f1e2d3c-4b5a-4968-8776-655443322110";
const UP2: &str = "0A1B2C3D-4E5F-4061-8273-849506172839";
const LOW2: &str = "0a1b2c3d-4e5f-4061-8273-849506172839";
const LOW3: &str = "ffffffff-0000-4000-8000-000000000000";

/// Criterion 2: `uuid` ids, declared, answered lowercase and ordered by their bytes.
#[tokio::test]
async fn uuid_ids_are_one_row_whatever_their_case() {
    let store = S::new(MemoryStore::new());
    let a = api(&store, 1);
    write(
        &a,
        "x",
        json!({"schema": {"id": "uuid", "owner": "uuid"},
               "documents": [doc(json!(UP), 1),
                             {"id": UP2, "vector": [1.0, 0.5], "attributes": {"n": 2, "owner": UP2}}]}),
    )
    .await;
    // An undeclared write in lowercase overwrites the same row.
    write(
        &a,
        "x",
        json!({"documents": [doc(json!(LOW), 3), doc(json!(LOW3.to_uppercase()), 4)]}),
    )
    .await;
    let check = |a: A| async move {
        assert_eq!(
            ns(&a, "x").await,
            [
                (json!(LOW2), json!(2)),
                (json!(LOW), json!(3)),
                (json!(LOW3), json!(4))
            ]
        );
        assert_eq!(
            ids(&a, "x", "desc", Value::Null).await,
            [json!(LOW3), json!(LOW), json!(LOW2)]
        );
        // Paging by `Gt` the last id answered visits every row once.
        let mut seen = Vec::new();
        let mut last: Option<Value> = None;
        loop {
            let mut q = json!({"rank_by": ["id", "asc"], "top_k": 1});
            if let Some(l) = &last {
                q["filters"] = json!(["id", "Gt", l]);
            }
            let page = query(&a, "x", &q).await["results"].clone();
            let Some(r) = page.as_array().unwrap().first() else {
                break;
            };
            last = Some(r["id"].clone());
            seen.push(r["id"].clone());
        }
        assert_eq!(seen, [json!(LOW2), json!(LOW), json!(LOW3)]);
        assert_eq!(
            ids(&a, "x", "asc", json!(["id", "Eq", UP])).await,
            [json!(LOW)]
        );
        assert!(
            ids(&a, "x", "asc", json!(["id", "Glob", "*"]))
                .await
                .is_empty()
        );
        assert!(ids(&a, "x", "asc", json!(["id", "Eq", 1])).await.is_empty());
    };
    check(Arc::clone(&a)).await;
    fold(&a).await;
    check(Arc::clone(&a)).await;
    check(api(&store, 1)).await;

    // `$ref_new: "id"` is the new row's uuid: it meets a uuid attribute holding it. Only
    // the row whose stored owner is its own id is written.
    write(
        &a,
        "x",
        json!({"patch_rows": [{"id": UP, "attributes": {"owner": LOW}},
                              {"id": UP2, "attributes": {"owner": LOW}}]}),
    )
    .await;
    write(
        &a,
        "x",
        json!({"durability": "applied",
               "upsert_condition": ["owner", "Eq", {"$ref_new": "id"}],
               "documents": [doc(json!(UP), 300), doc(json!(UP2), 200)]}),
    )
    .await;
    assert_eq!(
        ns(&a, "x").await,
        [
            (json!(LOW2), json!(2)),
            (json!(LOW), json!(300)),
            (json!(LOW3), json!(4))
        ]
    );
}

/// Criterion 3: what is refused, before anything is buffered.
#[tokio::test]
async fn ids_not_of_the_index_kind_are_refused() {
    let store = S::new(MemoryStore::new());
    let a = api(&store, 1);
    write(&a, "u", json!({"documents": [doc(json!(7), 1)]})).await;
    fold(&a).await;
    let a = api(&store, 1);
    let refused = |index: &'static str, body: Value, needle: &'static str| {
        let a = Arc::clone(&a);
        async move {
            let (s, b) = put(&a, index, &body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
            let msg = b["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            assert!(msg.contains(needle), "{body} -> {msg}");
            assert!(!msg.contains('\0'), "an id in its held form: {msg:?}");
            b
        }
    };
    for (id, needle) in [
        (json!("5"), "5"),
        (json!(-1), "-1"),
        (json!(1.5), "1.5"),
        (
            serde_json::from_str::<Value>("18446744073709551616").unwrap(),
            "1844674407370955",
        ),
    ] {
        refused("u", json!({"documents": [doc(id, 1)]}), needle).await;
    }
    write(
        &a,
        "x",
        json!({"schema": {"id": "uuid"}, "documents": [doc(json!(UP), 1)]}),
    )
    .await;
    refused(
        "x",
        json!({"documents": [doc(json!("not-a-uuid"), 1)]}),
        "not-a-uuid",
    )
    .await;
    refused("x", json!({"documents": [doc(json!(5), 1)]}), "5").await;
    let b = refused(
        "new",
        json!({"documents": [doc(json!(5), 1), doc(json!("five"), 2)]}),
        "five",
    )
    .await;
    assert_eq!(b["error"]["code"], "bad_request", "{b}");
    refused(
        "new",
        json!({"documents": [doc(json!("\u{0}u1"), 1)]}),
        "begins with NUL",
    )
    .await;
    refused(
        "new",
        json!({"schema": {"id": "datetime"}, "documents": [doc(json!(5), 1)]}),
        "datetime",
    )
    .await;
    // A kind other than the one known is a schema conflict: one recorded, and one this
    // process's own unfolded write gave.
    write(&a, "s", json!({"documents": [doc(json!("a"), 1)]})).await;
    fold(&a).await;
    let b = refused(
        "s",
        json!({"schema": {"id": "uint64"}, "documents": [doc(json!(5), 1)]}),
        "5",
    )
    .await;
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    write(&a, "fresh", json!({"documents": [doc(json!("a"), 1)]})).await;
    let b = refused("fresh", json!({"documents": [doc(json!(5), 1)]}), "5").await;
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    // A declared `string` is a kind: integer ids are refused under it, and it conflicts with
    // a recorded kind.
    let string = |id: Value| json!({"schema": {"id": "string"}, "documents": [doc(id, 1)]});
    let b = refused("new", string(json!(5)), "5").await;
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    let b = refused("u", string(json!("a")), "string").await;
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    // An engine refusal names the id as written too.
    refused(
        "new",
        json!({"distance_metric": "cosine_distance",
               "documents": [{"id": 5, "vector": [0.0, 0.0]}]}),
        "document 5",
    )
    .await;

    // A patch is read by the kind too, and its id joins the write's in one kind.
    let patch = |id: Value| json!({"patch_rows": [{"id": id, "attributes": {"n": 9}}]});
    let b = refused("u", patch(json!("five")), "five").await;
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    let b = refused("x", patch(json!(5)), "5").await;
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    let mut mixed = patch(json!("five"));
    mixed["documents"] = json!([doc(json!(5), 1)]);
    refused("new", mixed, "five").await;

    // Nothing of any refusal was buffered.
    fold(&a).await;
    assert_eq!(ids(&a, "u", "asc", Value::Null).await, [json!(7)]);
    assert_eq!(ids(&a, "x", "asc", Value::Null).await, [json!(LOW)]);
    assert_eq!(ids(&a, "fresh", "asc", Value::Null).await, [json!("a")]);
    let (s, _) = send(&a, "GET", "/v1/indexes/new", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// Criterion 4: two processes' first writes of two kinds; the first in lane order wins.
#[tokio::test]
async fn concurrent_first_writes_record_the_first_kind() {
    for ints_first in [true, false] {
        let store = S::new(MemoryStore::new());
        // Lane 1 is first in lane order, whichever writes first in time.
        let (one, two) = (api(&store, 1), api(&store, 2));
        let (first, second) = if ints_first {
            (json!(5), json!("five"))
        } else {
            (json!("five"), json!(5))
        };
        write(&two, "n", json!({"documents": [doc(second.clone(), 2)]})).await;
        write(&one, "n", json!({"documents": [doc(first.clone(), 1)]})).await;
        fold(&one).await;
        let meta = summary(&one, "n").await;
        let kind = if ints_first { "uint64" } else { "string" };
        assert_eq!(meta["schema"]["id"], kind, "{meta}");
        assert_eq!(meta["quarantined_rows"], 1, "{meta}");
        assert_eq!(
            ids(&one, "n", "asc", Value::Null).await,
            std::slice::from_ref(&first)
        );
        let (s, q) = send(&one, "GET", "/v1/indexes/n/quarantine", None).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(q["rows"][0]["document"]["id"], second, "{q}");

        // The losing process, once it reads HEAD, refuses its kind; a later fold of the
        // winning kind records nothing new.
        assert_eq!(
            ids(&two, "n", "asc", Value::Null).await,
            std::slice::from_ref(&first)
        );
        let (s, b) = put(&two, "n", &json!({"documents": [doc(second.clone(), 3)]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
        let more = if ints_first { json!(6) } else { json!("six") };
        write(&two, "n", json!({"documents": [doc(more.clone(), 4)]})).await;
        fold(&two).await;
        let meta = summary(&one, "n").await;
        assert_eq!(meta["schema"]["id"], kind, "{meta}");
        assert_eq!(meta["quarantined_rows"], 1, "{meta}");
        assert_eq!(ids(&one, "n", "asc", Value::Null).await, [first, more]);
    }
}

/// Criterion 3: a refusal by a kind this process holds re-reads HEAD before it stands: the
/// kind may be a dropped index's.
#[tokio::test]
async fn a_dropped_indexs_kind_is_read_again_before_it_refuses() {
    let store = S::new(MemoryStore::new());
    let (one, two) = (api(&store, 1), api(&store, 2));
    write(&one, "n", json!({"documents": [doc(json!(5), 1)]})).await;
    fold(&one).await;
    assert_eq!(summary(&one, "n").await["schema"]["id"], "uint64");
    let (s, b) = send(&two, "DELETE", "/v1/indexes/n", None).await;
    assert!(s.is_success(), "{b}");
    write(&two, "n", json!({"documents": [doc(json!("a"), 2)]})).await;
    fold(&two).await;
    // `one` still holds `n` as uint64; a string id with a value reads HEAD again.
    write(&one, "n", json!({"documents": [doc(json!("b"), 3)]})).await;
    fold(&one).await;
    assert_eq!(
        ids(&one, "n", "asc", Value::Null).await,
        [json!("a"), json!("b")]
    );
    // And a stale `uuid` kind rewrites no id of a batch it then refuses: the first id spells
    // a uuid, the second does not, and the fresh kind reads both as strings.
    write(
        &one,
        "x",
        json!({"schema": {"id": "uuid"}, "documents": [doc(json!(UP), 1)]}),
    )
    .await;
    fold(&one).await;
    let (s, b) = send(&two, "DELETE", "/v1/indexes/x", None).await;
    assert!(s.is_success(), "{b}");
    write(&two, "x", json!({"documents": [doc(json!("a"), 2)]})).await;
    fold(&two).await;
    write(
        &one,
        "x",
        json!({"documents": [doc(json!(UP), 3), doc(json!("plain"), 4)]}),
    )
    .await;
    fold(&one).await;
    assert_eq!(
        ids(&one, "x", "asc", Value::Null).await,
        [json!(UP), json!("a"), json!("plain")]
    );
}

/// Criterion 4: only the fold creating the schema records the kind. A process whose HEAD
/// predates that fold admits integer ids; the next fold quarantines them and records nothing.
#[tokio::test]
async fn a_later_fold_records_no_kind() {
    let store = S::new(MemoryStore::new());
    let (one, two) = (api(&store, 1), api(&store, 2));
    // `two` reads HEAD before `n` has a schema.
    write(&two, "other", json!({"documents": [doc(json!("z"), 1)]})).await;
    write(&one, "n", json!({"documents": [doc(json!("a"), 1)]})).await;
    fold(&one).await;
    write(&two, "n", json!({"documents": [doc(json!(5), 2)]})).await;
    fold(&two).await;
    let meta = summary(&one, "n").await;
    assert_eq!(meta["schema"]["id"], "string", "{meta}");
    assert_eq!(meta["quarantined_rows"], 1, "{meta}");
    assert_eq!(ids(&one, "n", "asc", Value::Null).await, [json!("a")]);
}

/// Criterion 5: the summary answers the kind, and a string index is unchanged.
#[tokio::test]
async fn the_summary_answers_the_id_kind() {
    let store = S::new(MemoryStore::new());
    let a = api(&store, 1);
    write(&a, "u", json!({"documents": [doc(json!(1), 1)]})).await;
    write(
        &a,
        "x",
        json!({"schema": {"id": "uuid"}, "documents": [doc(json!(UP), 1)]}),
    )
    .await;
    write(&a, "s", json!({"documents": [doc(json!("a"), 1)]})).await;
    write(&a, "later", json!({"documents": [doc(json!("a"), 1)]})).await;
    fold(&a).await;
    let kinds = |a: A| async move {
        for (index, kind) in [("u", "uint64"), ("x", "uuid"), ("s", "string")] {
            let meta = summary(&a, index).await;
            assert_eq!(meta["schema"]["id"], kind, "{index}: {meta}");
            assert_eq!(meta["schema"]["attributes"], json!({}), "{index}: {meta}");
        }
    };
    kinds(Arc::clone(&a)).await;
    kinds(api(&store, 1)).await;
    // An index created empty (M147) takes no schema in its body: its ids are strings.
    let (s, b) = send(&a, "PUT", "/v1/indexes/empty", Some(&json!({"dims": 2}))).await;
    assert!(s.is_success(), "{b}");
    assert_eq!(summary(&a, "empty").await["schema"]["id"], "string");
    let (s, b) = put(&a, "empty", &json!({"documents": [doc(json!(5), 1)]})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");

    // A branch inherits its parent's kind.
    let body = json!({"branch_from_namespace": "u"});
    let (s, b) = send(&a, "PUT", "/v1/indexes/child/documents", Some(&body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(summary(&a, "child").await["schema"]["id"], "uint64");
    assert_eq!(ids(&a, "child", "asc", Value::Null).await, [json!(1)]);

    // A dropped index re-created starts with no kind.
    let (s, b) = send(&a, "DELETE", "/v1/indexes/u", None).await;
    assert!(s.is_success(), "{b}");
    write(&a, "u", json!({"documents": [doc(json!("a"), 1)]})).await;
    fold(&a).await;
    assert_eq!(summary(&a, "u").await["schema"]["id"], "string");

    // An integer literal on a string index admits nothing.
    assert!(ids(&a, "s", "asc", json!(["id", "Eq", 1])).await.is_empty());
    assert_eq!(
        ids(&a, "s", "asc", json!(["id", "Eq", "a"])).await,
        [json!("a")]
    );
}
