//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M220 — **the native OpenAPI contract, held against the server that serves it.**
//!
//! `api/openapi.json` is written by hand and generated from nothing, so each claim it makes is
//! checked here against the server rather than trusted:
//!
//! - its paths, methods and scopes are the router's, both ways ([`route_table`]);
//! - each request body's properties are the fields the server's parser accepts, both ways,
//!   read from the parser's own refusal of a field it does not know;
//! - its error codes are the ones the server's source constructs, each at its status;
//! - every fixture step in `api/fixtures/native.json` -- the steps the generated clients run
//!   too -- answers the status it names, a body the operation's schema validates, and what its
//!   `expect` holds. Every operation is exercised, and every documented success status but
//!   the few [`UNEXERCISED`] names.
//!
//! ⚠️ What it cannot see: a query parameter the server reads and the contract does not name
//! (axum's `HashMap` extractor ignores an unknown one, so there is no refusal to read), a
//! multi-query root key the server adds (its refusal is prose, not serde's list), and an
//! error code built other than as `StatusCode::X, "code"`.

mod schema;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tower::ServiceExt;

use crate::support::auth;

const SPEC: &str = include_str!("../../../../api/openapi.json");
const FIXTURES: &str = include_str!("../../../../api/fixtures/native.json");
const METHODS: [&str; 5] = ["get", "put", "post", "patch", "delete"];

/// Documented success statuses no fixture can reach on a single in-memory server, each with
/// why. ⚠️ A row here is a claim the contract makes untested: keep it short.
const UNEXERCISED: &[(&str, &str, &str)] = &[
    (
        "register_external",
        "200",
        "needs an allowed external prefix and a Parquet or Lance object the store holds (M86)",
    ),
    ("drop_external", "200", "needs a registered external index"),
    ("query_external", "200", "needs a registered external index"),
    ("copy_external", "200", "needs a registered external index"),
    (
        "warm_index",
        "200",
        "the binary refuses a read cache over the memory store; validated on one in process by \
         `a_warm_answer_holds_the_contract`",
    ),
    (
        "copy_index",
        "200",
        "a repeated copy request: racing the background sync that ends the copy",
    ),
];

/// Request properties the server's parser accepts that the contract refuses, because the
/// handler refuses them by value. Each is a fixture step's 400.
const REFUSED_BY_VALUE: &[(&str, &[&str])] =
    &[("create_replication", &["read_only", "schema", "as_of"])];

fn spec() -> Value {
    serde_json::from_str(SPEC).expect("api/openapi.json parses")
}

#[derive(Debug, Clone)]
struct Op {
    id: String,
    method: String,
    path: String,
    scope: String,
    body: Option<Value>,
    /// Query parameters: name, whether required, schema.
    query: Vec<(String, bool, Value)>,
    responses: BTreeMap<String, Value>,
}

/// Every operation, in document order.
fn operations(spec: &Value) -> Vec<Op> {
    let mut out = Vec::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for m in METHODS {
            let Some(o) = item.get(m) else { continue };
            let scope = o["security"][0]["bearer"][0].as_str().unwrap().to_owned();
            let body = o
                .pointer("/requestBody/content/application~1json/schema")
                .cloned();
            let responses = o["responses"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(s, r)| {
                    let schema = r
                        .pointer("/content/application~1json/schema")
                        .or_else(|| r.pointer("/content/text~1plain/schema"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    (s.clone(), schema)
                })
                .collect();
            let query = o["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["in"] == "query")
                .map(|p| {
                    let name = p["name"].as_str().unwrap().to_owned();
                    (name, p["required"] == json!(true), p["schema"].clone())
                })
                .collect();
            out.push(Op {
                query,
                id: o["operationId"].as_str().unwrap().to_owned(),
                method: m.to_uppercase(),
                path: path.clone(),
                scope,
                body,
                responses,
            });
        }
    }
    out
}

fn fill(path: &str, params: &Value) -> String {
    let mut p = path.to_owned();
    for (k, v) in params.as_object().into_iter().flatten() {
        p = p.replace(&format!("{{{k}}}"), v.as_str().unwrap());
    }
    p
}

struct Answer {
    status: u16,
    body: Value,
    text: String,
}

async fn call(
    api: &Arc<Api<MemoryStore>>,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
    body: Option<&Value>,
) -> Answer {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let res = Arc::clone(api)
        .router()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    }
}

/// The server a fixture runs against: unsecured, no read cache, every applied-write level on.
/// What `scripts/clients.sh` starts with `PSTORE_APPLIED_WRITES=5`.
fn fixture_server() -> Arc<Api<MemoryStore>> {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    api.set_applied_writes(true);
    api.set_affected_ids(true);
    api.set_idempotency(true);
    api.set_partial_filters(true);
    api.set_filter_ids(true);
    api
}

/// The contract may use only keywords the validator checks: a new one is refused here,
/// before a response is ever validated against it.
#[test]
fn the_contract_uses_only_keywords_the_validator_checks() {
    let spec = spec();
    let mut bad = Vec::new();
    for (name, s) in spec["components"]["schemas"].as_object().unwrap() {
        schema::keywords_in(s, &format!("#/components/schemas/{name}"), &mut bad);
    }
    for (name, p) in spec["components"]["parameters"].as_object().unwrap() {
        schema::keywords_in(&p["schema"], &format!("parameters.{name}"), &mut bad);
    }
    for op in operations(&spec) {
        if let Some(b) = &op.body {
            schema::keywords_in(b, &format!("{}.requestBody", op.id), &mut bad);
        }
        for (name, _, q) in &op.query {
            schema::keywords_in(q, &format!("{}.query.{name}", op.id), &mut bad);
        }
        for (status, s) in &op.responses {
            if !s.is_null() {
                schema::keywords_in(s, &format!("{}.{status}", op.id), &mut bad);
            }
        }
    }
    // Every $ref in the document resolves.
    fn refs(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                if let Some(Value::String(r)) = o.get("$ref") {
                    out.push(r.clone());
                }
                o.values().for_each(|v| refs(v, out));
            }
            Value::Array(a) => a.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }
    let mut all = Vec::new();
    refs(&spec, &mut all);
    for r in all {
        if spec.pointer(&r[1..]).is_none() {
            bad.push(format!("{r} does not resolve"));
        }
    }
    let ids: Vec<_> = operations(&spec).into_iter().map(|o| o.id).collect();
    let unique: BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "operationIds repeat: {ids:?}");
    assert!(bad.is_empty(), "{bad:#?}");
}

/// The validator rejects a value each assertion keyword forbids, so a green fixture run
/// means the schema was checked, not skipped.
#[test]
fn the_validator_refuses_what_each_keyword_forbids() {
    let root = json!({"components": {"schemas": {"N": {"type": "integer"}}}});
    let cases = [
        (json!({"type": "integer"}), json!(1), json!(1.5)),
        (json!({"type": ["string", "null"]}), json!(null), json!(1)),
        (json!({"enum": ["a"]}), json!("a"), json!("b")),
        (json!({"minimum": 1}), json!(1), json!(0)),
        (json!({"maximum": 1}), json!(1), json!(2)),
        (json!({"minItems": 1}), json!([1]), json!([])),
        (json!({"maxItems": 1}), json!([1]), json!([1, 2])),
        (
            json!({"items": {"type": "string"}}),
            json!(["a"]),
            json!([1]),
        ),
        (json!({"required": ["a"]}), json!({"a": 1}), json!({})),
        (
            json!({"properties": {"a": {"type": "string"}}}),
            json!({"a": "x"}),
            json!({"a": 1}),
        ),
        (
            json!({"properties": {}, "additionalProperties": false}),
            json!({}),
            json!({"a": 1}),
        ),
        (
            json!({"additionalProperties": {"type": "string"}}),
            json!({"a": "x"}),
            json!({"a": 1}),
        ),
        (
            json!({"oneOf": [{"type": "string"}, {"type": "integer"}]}),
            json!(1),
            json!(null),
        ),
        (
            json!({"oneOf": [{"type": "number"}, {"type": "integer"}]}),
            json!(1.5),
            json!(1),
        ),
        (
            json!({"$ref": "#/components/schemas/N"}),
            json!(1),
            json!("1"),
        ),
        (
            json!({"type": "string", "format": "date-time"}),
            json!("2026-11-09T11:40:35.825Z"),
            json!("yesterday"),
        ),
    ];
    for (s, good, bad) in cases {
        assert!(
            schema::errors(&root, &s, &good).is_empty(),
            "{s} refused {good}"
        );
        assert!(
            !schema::errors(&root, &s, &bad).is_empty(),
            "{s} accepted {bad}"
        );
    }
    let mut unknown = Vec::new();
    schema::keywords_in(&json!({"pattern": "^a"}), "$", &mut unknown);
    assert_eq!(unknown.len(), 1, "an unchecked keyword passes: {unknown:?}");
}

/// The router's every `(path, scope)` is the contract's, and so is each method under it.
///
/// A method is served under a scope when a token holding only that scope is neither refused
/// `405` (no such method) nor `403 forbidden` (another scope's) -- a handler's own `403`,
/// such as `external_sources_off`, still means the route ran.
#[tokio::test]
async fn every_route_is_in_the_contract_under_its_scope() {
    let spec = spec();
    let config = auth::config(&[("current", &auth::KEY)], "current");
    let (api, _) = auth::api(&config);
    let served: BTreeSet<(String, String)> = api
        .route_table()
        .into_iter()
        .map(|(p, s)| (p, s.to_owned()))
        .collect();
    let ops = operations(&spec);
    let documented: BTreeSet<(String, String)> = ops
        .iter()
        .map(|o| (o.path.clone(), o.scope.clone()))
        .collect();
    assert_eq!(
        served, documented,
        "the router's (path, scope) pairs: a route added or moved names itself in api/openapi.json"
    );
    let mut found = BTreeSet::new();
    for (path, scope) in &served {
        let bearer = format!("Bearer {}", auth::token("7", &[scope.as_str()]));
        let uri = path.replace("{index}", "openapi_probe");
        for m in METHODS {
            let m = m.to_uppercase();
            let a = call(&api, &m, &uri, &[("authorization", bearer.clone())], None).await;
            if a.status != 405 && a.body["error"]["code"] != "forbidden" {
                found.insert((m, path.clone(), scope.clone()));
            }
        }
    }
    let want: BTreeSet<_> = ops
        .iter()
        .map(|o| (o.method.clone(), o.path.clone(), o.scope.clone()))
        .collect();
    assert_eq!(
        found, want,
        "(method, path, scope) the server serves: api/openapi.json names each"
    );
}

/// The names in serde's refusal of an unknown field: `expected one of `a`, `b``,
/// `expected `a` or `b``, `expected `a``.
fn expected_fields(message: &str) -> BTreeSet<String> {
    let tail = message
        .split_once(", expected ")
        .unwrap_or_else(|| panic!("not serde's unknown-field refusal: {message}"))
        .1;
    tail.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// Where each closed request object is probed: the operation, the schema, and a body holding
/// `"openapi_probe"` as an unknown key inside that object.
fn probes() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("create_index", "CreateIndexRequest", json!({})),
        ("patch_index", "PatchIndexRequest", json!({})),
        ("write_documents", "WriteRequest", json!({})),
        (
            "write_documents",
            "Document",
            json!({"documents": [{"id": "x", "vector": [0.0, 0.0]}]}),
        ),
        ("query", "QueryRequest", json!({})),
        (
            "query",
            "MultiVectorQuery",
            json!({"multi": {"field": "m", "vectors": [[0.0]]}}),
        ),
        ("recall", "RecallRequest", json!({})),
        ("register_external", "ExternalRegisterRequest", json!({})),
        ("query_external", "ExternalQueryRequest", json!({})),
        ("copy_external", "ExternalCopyRequest", json!({})),
        ("create_replication", "ReplicationRequest", json!({})),
        ("copy_index", "CopyRequest", json!({})),
        ("copy_index", "ReplicaSource", json!({"source": {}})),
    ]
}

/// Puts the unknown key into the innermost object of `v` (the probe's target): following an
/// object member that holds an object, or an array whose first item is one.
fn plant(v: &mut Value) {
    let o = v.as_object_mut().unwrap();
    let inner = o.values_mut().find_map(|e| match e {
        Value::Object(_) => Some(e),
        Value::Array(a) => a.first_mut().filter(|f| f.is_object()),
        _ => None,
    });
    match inner {
        Some(e) => plant(e),
        None => {
            o.insert("openapi_probe".into(), json!(1));
        }
    }
}

/// Each closed request object's properties are what the server's parser accepts: the parser
/// names its fields when it refuses one it does not know.
#[tokio::test]
async fn request_bodies_name_the_fields_the_server_parses() {
    let spec = spec();
    let ops = operations(&spec);
    let api = fixture_server();
    // Each probed object must be found under the operation's body, so a probe list naming a
    // schema the operation does not take fails rather than probing nothing.
    let mut probed = BTreeSet::new();
    for (op_id, name, mut body) in probes() {
        let op = ops.iter().find(|o| o.id == op_id).unwrap();
        plant(&mut body);
        let a = call(
            &api,
            &op.method,
            &op.path.replace("{index}", "openapi_probe"),
            &[("x-pstore-tenant", "7".to_owned())],
            Some(&body),
        )
        .await;
        assert_eq!(a.status, 400, "{op_id} {name}: {}", a.text);
        let server = expected_fields(a.body["error"]["message"].as_str().unwrap());
        let s = &spec["components"]["schemas"][name];
        assert_eq!(s["additionalProperties"], json!(false), "{name} is closed");
        let mut documented: BTreeSet<String> = s["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        if name.ends_with("Request") {
            for (o, extra) in REFUSED_BY_VALUE {
                if *o == op_id {
                    documented.extend(extra.iter().map(|e| (*e).to_owned()));
                }
            }
        }
        assert_eq!(
            server, documented,
            "{op_id}: {name}'s fields: a field the server parses is a property in api/openapi.json"
        );
        probed.insert(name);
    }
    // Every closed object reachable from a request body is probed, so a new closed request
    // object cannot go unchecked.
    fn closed(spec: &Value, s: &Value, seen: &mut BTreeSet<String>) {
        if let Some(r) = s.get("$ref").and_then(Value::as_str) {
            let name = r.rsplit('/').next().unwrap().to_owned();
            if seen.insert(name) {
                closed(spec, spec.pointer(&r[1..]).unwrap(), seen);
            }
            return;
        }
        for k in ["items", "additionalProperties"] {
            if let Some(v) = s.get(k).filter(|v| v.is_object()) {
                closed(spec, v, seen);
            }
        }
        for v in s["properties"].as_object().into_iter().flatten() {
            closed(spec, v.1, seen);
        }
        for v in s["oneOf"].as_array().into_iter().flatten() {
            closed(spec, v, seen);
        }
    }
    let mut reachable = BTreeSet::new();
    for op in &ops {
        if let Some(b) = &op.body {
            closed(&spec, b, &mut reachable);
        }
    }
    let unprobed: Vec<_> = reachable
        .iter()
        .filter(|n| {
            spec["components"]["schemas"][n.as_str()]["additionalProperties"] == json!(false)
                && !probed.contains(n.as_str())
        })
        .collect();
    // ⚠️ The multi-query root refuses in prose, not serde's list: its keys are probed one by
    // one in the fixtures instead (an unknown root key is refused, each named one is not).
    assert_eq!(
        unprobed,
        vec!["MultiQueryRequest"],
        "closed request objects never probed"
    );
}

/// The error codes the server's source constructs, each at the status it is constructed
/// with, are the contract's. Peer-to-peer modules are internal and not the public API.
#[test]
fn error_codes_are_the_servers() {
    let spec = spec();
    let documented: BTreeMap<String, u64> =
        spec["components"]["schemas"]["ErrorCode"]["x-pstore-status"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_u64().unwrap()))
            .collect();
    let listed: BTreeSet<String> = spec["components"]["schemas"]["ErrorCode"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        listed,
        documented.keys().cloned().collect::<BTreeSet<_>>(),
        "the enum and its statuses"
    );
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                if !name.starts_with("peer") {
                    stack.push(p);
                }
                continue;
            }
            if name.starts_with("peer") || name == "tests.rs" || !name.ends_with(".rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            for (status, code) in status_codes(&text) {
                found.entry(code).or_default().insert(status);
            }
        }
    }
    let found: BTreeMap<String, u64> = found
        .into_iter()
        .map(|(code, s)| {
            assert_eq!(s.len(), 1, "{code} is constructed at two statuses: {s:?}");
            (code, s.into_iter().next().unwrap())
        })
        .collect();
    assert_eq!(
        documented, found,
        "error code -> status: a new code is in api/openapi.json's ErrorCode, with its status"
    );
}

/// Every `StatusCode::NAME, "code"` in `text`, as `(status, code)`.
fn status_codes(text: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("StatusCode::") {
        rest = &rest[i + "StatusCode::".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || *c == '_')
            .collect();
        let after = rest[name.len()..].trim_start();
        let Some(after) = after.strip_prefix(',') else {
            continue;
        };
        let after = after.trim_start();
        let Some(after) = after.strip_prefix('"') else {
            continue;
        };
        let code: String = after
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '_')
            .collect();
        if !after[code.len()..].starts_with('"') || code.is_empty() {
            continue;
        }
        out.push((status_by_name(&name), code));
    }
    out
}

/// A `StatusCode` constant's number, by its name.
fn status_by_name(name: &str) -> u64 {
    (100..600)
        .filter_map(|n| axum::http::StatusCode::from_u16(n).ok())
        .find(|s| {
            s.canonical_reason()
                .is_some_and(|r| r.to_uppercase().replace([' ', '-'], "_") == name)
        })
        .map_or_else(
            || panic!("no status named {name}"),
            |s| u64::from(s.as_u16()),
        )
}

/// `got` holds `want`: an object at least its keys, an array exactly its items in order, a
/// number equal as a number, anything else equal.
fn holds(got: &Value, want: &Value, at: &str, out: &mut Vec<String>) {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            for (k, wv) in w {
                match g.get(k) {
                    Some(gv) => holds(gv, wv, &format!("{at}.{k}"), out),
                    None => out.push(format!("{at}.{k} is missing")),
                }
            }
        }
        (Value::Array(g), Value::Array(w)) => {
            if g.len() == w.len() {
                for (i, (gv, wv)) in g.iter().zip(w).enumerate() {
                    holds(gv, wv, &format!("{at}[{i}]"), out);
                }
            } else {
                out.push(format!("{at}: {} items, not {}", g.len(), w.len()));
            }
        }
        (Value::Number(g), Value::Number(w)) if g.as_f64() == w.as_f64() => {}
        _ if got == want => {}
        _ => out.push(format!("{at}: {got}, not {want}")),
    }
}

/// Every way a step's query fails its operation's parameters: a name the contract does not
/// declare, a required one missing, or a value its schema refuses -- read as the server reads
/// a query string, an integer or a boolean parsed from its text.
fn query_errors(spec: &Value, op: &Op, q: &Value) -> Vec<String> {
    let given = q.as_object().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for k in given.keys() {
        if !op.query.iter().any(|(n, _, _)| n == k) {
            out.push(format!("query {k:?} is not a parameter of {}", op.id));
        }
    }
    for (n, required, s) in &op.query {
        let Some(raw) = given.get(n) else {
            if *required {
                out.push(format!("required query {n:?} is missing"));
            }
            continue;
        };
        let text = raw.as_str().map_or_else(|| raw.to_string(), str::to_owned);
        let v = match s["type"].as_str() {
            Some("integer") => text.parse::<u64>().map_or(Value::String(text), Value::from),
            Some("boolean") => match text.as_str() {
                "true" => json!(true),
                "false" => json!(false),
                _ => Value::String(text),
            },
            _ => Value::String(text),
        };
        out.extend(
            schema::errors(spec, s, &v)
                .into_iter()
                .map(|e| format!("query {n}: {e}")),
        );
    }
    out
}

/// Every fixture step against the server: its status, its body against the contract, and its
/// `expect`. Then every operation, and every documented success status but [`UNEXERCISED`]'s,
/// was answered at least once.
#[tokio::test]
async fn the_fixtures_hold_against_the_server_and_the_contract() {
    let spec = spec();
    let ops = operations(&spec);
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    let tenant = fixtures["tenant"].as_str().unwrap().to_owned();
    let statuses = &spec["components"]["schemas"]["ErrorCode"]["x-pstore-status"];
    let api = fixture_server();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut failures = Vec::new();
    for step in fixtures["steps"].as_array().unwrap() {
        let name = step["name"].as_str().unwrap();
        let op_id = step["operation"].as_str().unwrap();
        let Some(op) = ops.iter().find(|o| o.id == op_id) else {
            failures.push(format!("{name}: no operation {op_id}"));
            continue;
        };
        let mut uri = fill(&op.path, &step["path"]);
        if let Some(q) = step["query"].as_object() {
            let pairs: Vec<String> = q
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{k}={}",
                        v.as_str().map_or_else(|| v.to_string(), str::to_owned)
                    )
                })
                .collect();
            uri = format!("{uri}?{}", pairs.join("&"));
        }
        let body = step.get("body");
        // A step's request is the contract's -- or, marked `invalid`, deliberately not: then
        // the contract must refuse it too, so a schema that accepts what the server refuses
        // fails. Its body and its query are both read.
        let mut errs = Vec::new();
        if let (Some(b), Some(s)) = (body, &op.body) {
            errs.extend(schema::errors(&spec, s, b));
        }
        errs.extend(query_errors(&spec, op, &step["query"]));
        match step["invalid"].as_bool().unwrap_or(false) {
            false if !errs.is_empty() => failures.push(format!("{name}: request {errs:?}")),
            true if errs.is_empty() => {
                failures.push(format!("{name}: an invalid request the contract accepts"));
            }
            _ => {}
        }
        let a = call(
            &api,
            &op.method,
            &uri,
            &[("x-pstore-tenant", tenant.clone())],
            body,
        )
        .await;
        let want = step["status"].as_u64().unwrap();
        if u64::from(a.status) != want {
            failures.push(format!("{name}: {} not {want}: {}", a.status, a.text));
            continue;
        }
        let status = a.status.to_string();
        let schema = op
            .responses
            .get(&status)
            .or_else(|| op.responses.get("default"))
            .unwrap();
        let value = if schema["type"] == "string" {
            Value::String(a.text.clone())
        } else {
            a.body.clone()
        };
        for e in schema::errors(&spec, schema, &value) {
            failures.push(format!("{name}: {e}"));
        }
        if a.status >= 400 {
            let code = a.body["error"]["code"].as_str().unwrap_or_default();
            if statuses[code] != json!(a.status) {
                failures.push(format!("{name}: {code} answered {}", a.status));
            }
        }
        if let Some(e) = step.get("expect") {
            holds(&a.body, e, name, &mut failures);
        }
        for ptr in step["absent"].as_array().into_iter().flatten() {
            let ptr = ptr.as_str().unwrap();
            if let Some(v) = a.body.pointer(ptr) {
                failures.push(format!("{name}: {ptr} is {v}, and must be absent"));
            }
        }
        seen.insert((op_id.to_owned(), status));
    }
    assert!(
        failures.is_empty(),
        "api/fixtures/native.json against the server and api/openapi.json: {failures:#?}"
    );
    let mut missing = Vec::new();
    for op in &ops {
        if !seen.iter().any(|(o, _)| *o == op.id) {
            missing.push(format!("{}: no step", op.id));
        }
        for status in op.responses.keys().filter(|s| s.starts_with('2')) {
            let excused = UNEXERCISED
                .iter()
                .any(|(o, s, _)| *o == op.id && s == status);
            let ran = seen.contains(&(op.id.clone(), status.clone()));
            if ran == excused {
                missing.push(format!(
                    "{} {status}: {}",
                    op.id,
                    if ran {
                        "exercised, and still excused"
                    } else {
                        "never answered"
                    }
                ));
            }
        }
    }
    assert!(missing.is_empty(), "{missing:#?}");
}

/// `warm_index`'s success, on a server with a read cache, which the fixtures' server has not.
#[tokio::test]
async fn a_warm_answer_holds_the_contract() {
    let spec = spec();
    let core = Arc::new(pstore_cache::CacheCore::memory(64 << 20));
    let api =
        Api::with_cache_unsecured(Accounted::new(MemoryStore::new()), LaneId(1), core).unwrap();
    let tenant = [("x-pstore-tenant", "7".to_owned())];
    let write = json!({"durability": "durable", "documents": [{"id": "a", "vector": [0.0]}]});
    let w = call(
        &api,
        "PUT",
        "/v1/indexes/w/documents",
        &tenant,
        Some(&write),
    )
    .await;
    assert_eq!(w.status, 200, "{}", w.text);
    assert_eq!(
        call(&api, "POST", "/v1/admin/fold", &tenant, None)
            .await
            .status,
        200
    );
    let a = call(&api, "POST", "/v1/indexes/w/warm", &tenant, None).await;
    assert_eq!(a.status, 200, "{}", a.text);
    let op = operations(&spec)
        .into_iter()
        .find(|o| o.id == "warm_index")
        .unwrap();
    assert_eq!(
        schema::errors(&spec, &op.responses["200"], &a.body),
        Vec::<String>::new()
    );
    assert_eq!(a.body["segments"], 1, "{}", a.text);
}
