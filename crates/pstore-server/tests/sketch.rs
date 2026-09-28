//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The trigram sketch (M15.2): declared with `"regex": true`, it prunes blocks for `Glob`,
//! `IGlob`, `Regex` and `Fuzzy` -- and never changes an answer.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

fn apis(lanes: &[u64]) -> (Accounted<MemoryStore>, Vec<A>) {
    let store = Accounted::new(MemoryStore::new());
    let apis = lanes
        .iter()
        .map(|l| Api::new(store.clone(), LaneId(*l)).unwrap())
        .collect();
    (store, apis)
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "47")
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

async fn put(api: &A, body: Value) -> (StatusCode, Value) {
    let mut body = body;
    body["durability"] = json!("durable");
    send(api, "PUT", "/v1/indexes/docs/documents", &body).await
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// The ids a filter admits, and the block bytes the query read.
async fn run(api: &A, f: &Value) -> (BTreeSet<String>, u64) {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10_000, "filters": f});
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{f} -> {b}");
    let ids = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    (ids, b["meta"]["cost"]["bytes_read"].as_u64().unwrap())
}

const ALPHABET: &[char] = &[
    'a', 'b', 'c', 'A', 'B', 's', 'S', 'ſ', 'σ', 'ς', 'Σ', 'µ', 'μ', 'Μ', 'k', 'K', '\u{212A}',
    'İ', 'i', '/', '\n', ' ', '.', 'x', 'y',
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick(&mut self, from: &[char]) -> char {
        from[(self.next() % from.len() as u64) as usize]
    }
    fn word(&mut self, lo: usize, hi: usize) -> String {
        let len = lo + (self.next() as usize % (hi - lo + 1));
        (0..len).map(|_| self.pick(ALPHABET)).collect()
    }
}

/// Row `i`'s value; row 777 alone holds letters no other row has.
fn value(i: usize) -> String {
    if i == 777 {
        return "zzqqvv".to_owned();
    }
    Rng(0x1234_5678 ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1).word(3, 10)
}

fn casefold(c: char) -> char {
    match c {
        'S' | 'ſ' => 's',
        'ς' | 'Σ' => 'σ',
        'µ' | 'Μ' => 'μ',
        'K' | '\u{212A}' => 'k',
        c if c.is_ascii_uppercase() => c.to_ascii_lowercase(),
        c => c,
    }
}

fn glob(p: &[char], s: &[char], ci: bool) -> bool {
    let eq = |a: char, b: char| {
        if ci {
            casefold(a) == casefold(b)
        } else {
            a == b
        }
    };
    match p.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| glob(&p[1..], &s[i..], ci)),
        Some('?') => !s.is_empty() && glob(&p[1..], &s[1..], ci),
        Some(&c) => !s.is_empty() && eq(c, s[0]) && glob(&p[1..], &s[1..], ci),
    }
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(here + 1);
            prev = here;
        }
    }
    row[b.len()]
}

type Oracle = Box<dyn Fn(&str) -> bool>;

/// `n` generated patterns of every kind over attribute `attr`, with their oracles.
fn patterns(n: usize, attr: &str) -> Vec<(Value, Oracle)> {
    let mut r = Rng(42);
    let mut out: Vec<(Value, Oracle)> = Vec::new();
    for i in 0..n {
        match i % 5 {
            0 | 1 => {
                // A glob of literal runs, `*` and `?`, anchored or not by its ends.
                let mut g = String::new();
                if r.next().is_multiple_of(2) {
                    g.push('*');
                }
                g.push_str(&r.word(1, 4));
                if r.next().is_multiple_of(3) {
                    g.push('?');
                    g.push_str(&r.word(1, 3));
                }
                g.push('*');
                let ci = i % 5 == 1;
                let p: Vec<char> = g.chars().collect();
                let op = if ci { "IGlob" } else { "Glob" };
                out.push((
                    json!([attr, op, g]),
                    Box::new(move |s| glob(&p, &s.chars().collect::<Vec<_>>(), ci)),
                ));
            }
            2 | 3 => {
                // A regex whose literal runs are broken by a repetition, a class or an
                // alternation -- the joins a careless extraction would make.
                let (a, b) = (r.word(2, 4), r.word(1, 3));
                let shapes = [
                    format!("{}{}", regex::escape(&a), regex::escape(&b)),
                    format!(
                        "{}({})+{}",
                        regex::escape(&a),
                        regex::escape(&b),
                        regex::escape(&a)
                    ),
                    format!("{}|{}", regex::escape(&a), regex::escape(&b)),
                    format!("(?i){}", regex::escape(&a)),
                    format!("{}[a-c]{}", regex::escape(&a), regex::escape(&b)),
                ];
                let src = shapes[(r.next() % shapes.len() as u64) as usize].clone();
                let re = regex::Regex::new(&src).unwrap();
                out.push((
                    json!([attr, "Regex", src]),
                    Box::new(move |s| re.is_match(s)),
                ));
            }
            _ => {
                let v = r.word(3, 6);
                let k = (r.next() % 3) as usize;
                let vc: Vec<char> = v.chars().collect();
                out.push((
                    json!([attr, "Fuzzy", {"value": v, "max_edits": k}]),
                    Box::new(move |s| levenshtein(&s.chars().collect::<Vec<_>>(), &vc) <= k),
                ));
            }
        }
    }
    out
}

fn declared() -> Value {
    json!({"s": {"type": "string", "regex": true}})
}

/// 2,000 rows in 4 folds: `s` declared, `u` the same values undeclared.
async fn seeded(a: &A) -> Vec<(String, String)> {
    let rows: Vec<(String, String)> = (0..2000).map(|i| (format!("d{i:05}"), value(i))).collect();
    for chunk in rows.chunks(500) {
        let docs: Vec<Value> = chunk
            .iter()
            .map(|(id, v)| json!({"id": id, "vector": [1.0, 0.5], "attributes": {"s": v, "u": v}}))
            .collect();
        let (s, b) = put(a, json!({"schema": declared(), "documents": docs})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        fold(a).await;
    }
    rows
}

#[tokio::test]
async fn a_sketch_never_changes_an_answer() {
    let (_, w) = apis(&[1]);
    let a = &w[0];
    let rows = seeded(a).await;
    for ((f, oracle), (g, _)) in patterns(300, "s").into_iter().zip(patterns(300, "u")) {
        let want: BTreeSet<String> = rows
            .iter()
            .filter(|(_, v)| oracle(v))
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(run(a, &f).await.0, want, "declared {f}");
        assert_eq!(run(a, &g).await.0, want, "undeclared {g}");
    }
}

#[tokio::test]
async fn a_sketch_prunes_a_selective_pattern() {
    let (_, w) = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    let one = BTreeSet::from(["d00777".to_owned()]);
    for (op, arg) in [
        ("Glob", json!("zzq*")),
        ("IGlob", json!("*QQV*")),
        ("Regex", json!("qqv")),
        ("Fuzzy", json!({"value": "zzqqvx", "max_edits": 1})),
    ] {
        let (hit, declared) = run(a, &json!(["s", op, arg])).await;
        let (same, undeclared) = run(a, &json!(["u", op, arg])).await;
        assert_eq!(hit, one, "{op} {arg}");
        assert_eq!(same, one, "{op} {arg}");
        assert!(
            declared < undeclared,
            "{op} {arg}: {declared} bytes declared, {undeclared} undeclared"
        );
    }
}

#[tokio::test]
async fn a_declaration_is_fixed_when_its_index_is_created() {
    let (store, w) = apis(&[1]);
    let a = &w[0];
    let doc =
        |id: &str| json!({"id": id, "vector": [1.0, 0.5], "attributes": {"s": "abc", "u": "abc"}});
    let (s, b) = put(a, json!({"schema": declared(), "documents": [doc("1")]})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(a).await;
    for other in [
        json!({"u": {"type": "string", "regex": true}}),
        json!({"s": {"type": "string", "regex": true}, "u": {"type": "string", "regex": true}}),
    ] {
        let (s, b) = put(a, json!({"schema": other, "documents": [doc("2")]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{other}: {b}");
        assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains("reindex") && msg.contains("copy"), "{msg}");
    }
    // Undeclared, and `regex: false`, have no opinion.
    let (s, b) = put(a, json!({"documents": [doc("3")]})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = put(
        a,
        json!({"schema": {"s": {"type": "string", "regex": false}}, "documents": [doc("4")]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(a).await;
    // A new process sees the set.
    let fresh = Api::new(store.clone(), LaneId(9)).unwrap();
    let (s, b) = send(&fresh, "GET", "/v1/indexes/docs", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["schema"]["regex"], json!(["s"]), "{b}");
    // `id` cannot be declared.
    let (s, b) = put(
        a,
        json!({"schema": {"id": {"type": "string", "regex": true}}, "documents": [doc("5")]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn a_fuzzy_match_at_distance_k_is_never_pruned() {
    // The q-gram bound's tight case (spec review, M15): one row, alone among values too short
    // to have trigrams, at distance exactly 2 from the query with its edits spread out -- which
    // destroys 6 of its 8 trigrams and leaves exactly `8 - 3k`.
    let (_, w) = apis(&[1]);
    let a = &w[0];
    let docs: Vec<Value> = (0..200)
        .map(|i| {
            let v = if i == 150 {
                "abcdefghij".to_owned()
            } else {
                (i % 10).to_string()
            };
            json!({"id": format!("d{i:05}"), "vector": [1.0, 0.5], "attributes": {"s": v}})
        })
        .collect();
    let (s, b) = put(a, json!({"schema": declared(), "documents": docs})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(a).await;
    let f = json!(["s", "Fuzzy", {"value": "abXdefgYij", "max_edits": 2}]);
    assert_eq!(run(a, &f).await.0, BTreeSet::from(["d00150".to_owned()]));
}
