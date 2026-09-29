//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `Glob`, `IGlob`, `Regex` and `Fuzzy` (M15.1), evaluated per row, checked against oracles
//! that share no code with the implementation: a glob matcher and a Levenshtein written here,
//! and the `regex` crate called directly.

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

fn api() -> A {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "46")
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

async fn write(api: &A, mut body: Value) {
    body["durability"] = json!("durable");
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn ids(api: &A, f: &Value) -> BTreeSet<String> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10_000, "filters": f});
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{f} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Characters chosen to break a careless fold: `ſ`, `ς`/`Σ`, `µ`/`μ`, Kelvin `K`, `İ`, and a
/// newline and a slash for a careless glob.
const ALPHABET: &[char] = &[
    'a', 'b', 'c', 'A', 'B', 's', 'S', 'ſ', 'σ', 'ς', 'Σ', 'µ', 'μ', 'Μ', 'k', 'K', '\u{212A}',
    'İ', 'i', '/', '\n', ' ', '.', '*', 'x', ']', '-', '+',
];

fn string(i: usize) -> String {
    let mut x = (i as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(7);
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let len = 2 + (next() % 9) as usize;
    (0..len)
        .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize])
        .collect()
}

/// Simple case folding for this alphabet, written out: the classes `(?i)` treats as one.
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

/// A glob matched against the whole of `s`: `*`, `?`, `[..]`, `[!..]`, and `\` escapes.
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
        Some('[') => {
            // The first member, after an optional `!`, may be `]`.
            let start = if p[1] == '!' { 2 } else { 1 };
            let close = p.iter().skip(start + 1).position(|c| *c == ']').unwrap() + start + 1;
            let (neg, body) = (start == 2, &p[start..close]);
            let Some(&c) = s.first() else { return false };
            let mut hit = false;
            let mut k = 0;
            while k < body.len() {
                if k + 2 < body.len() && body[k + 1] == '-' {
                    let (lo, hi) = (body[k], body[k + 2]);
                    hit |= (lo..=hi).contains(&c) || (ci && (lo..=hi).any(|x| eq(x, c)));
                    k += 3;
                } else {
                    hit |= eq(body[k], c);
                    k += 1;
                }
            }
            hit != neg && glob(&p[close + 1..], &s[1..], ci)
        }
        Some('\\') => !s.is_empty() && eq(p[1], s[0]) && glob(&p[2..], &s[1..], ci),
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

/// A filter, and whether it holds for a string value.
/// A filter and its oracle.
type Case = (Value, Box<dyn Fn(&str) -> bool>);

fn cases() -> Vec<Case> {
    let mut out: Vec<Case> = Vec::new();
    let globs = [
        "a*", "*b*", "?c*", "[ab]*", "[!a]*", "[a-c]?*", "*\\**", "*/*", "*\n*", "ſ*", "*σ*", "K*",
        "*İ", "s?k", "*", "", "[]x]*", "[!]x]*", "[]]a*", "[+--]*", "*[a-]",
    ];
    for g in globs {
        let p: Vec<char> = g.chars().collect();
        let (p1, p2) = (p.clone(), p.clone());
        out.push((
            json!(["s", "Glob", g]),
            Box::new(move |s| glob(&p1, &s.chars().collect::<Vec<_>>(), false)),
        ));
        out.push((
            json!(["s", "IGlob", g]),
            Box::new(move |s| glob(&p2, &s.chars().collect::<Vec<_>>(), true)),
        ));
    }
    for r in [
        "ab", "^a", "c$", "s.k", "(?i)σ", "(?i)K", "[a-c]{2}", "a|b", "(?s)a.b", "^$",
    ] {
        let re = regex::Regex::new(r).unwrap();
        out.push((json!(["s", "Regex", r]), Box::new(move |s| re.is_match(s))));
    }
    for (v, k) in [("abc", 0), ("abc", 1), ("ſσk", 1), ("aSb", 2), ("x", 2)] {
        let vc: Vec<char> = v.chars().collect();
        out.push((
            json!(["s", "Fuzzy", {"value": v, "max_edits": k}]),
            Box::new(move |s| levenshtein(&s.chars().collect::<Vec<_>>(), &vc) <= k),
        ));
    }
    out
}

async fn seeded(a: &A, n: usize) -> Vec<(String, String)> {
    let rows: Vec<(String, String)> = (0..n).map(|i| (format!("d{i:05}"), string(i))).collect();
    for chunk in rows.chunks(500) {
        let docs: Vec<Value> = chunk
            .iter()
            .map(|(id, s)| {
                json!({"id": id, "vector": [1.0, 0.5],
                       "attributes": {"s": s, "arr": [s], "n": 3}})
            })
            .collect();
        write(a, json!({"documents": docs})).await;
        fold(a).await;
    }
    rows
}

#[tokio::test]
async fn each_pattern_filter_equals_its_oracle() {
    let a = api();
    let rows = seeded(&a, 2000).await;
    for (f, holds) in cases() {
        let want: BTreeSet<String> = rows
            .iter()
            .filter(|(_, s)| holds(s))
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(ids(&a, &f).await, want, "{f}");
        let not: BTreeSet<String> = rows
            .iter()
            .filter(|(_, s)| !holds(s))
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(ids(&a, &json!(["Not", f])).await, not, "Not {f}");
        // An array of strings and a number admit nothing.
        for attr in ["arr", "n", "missing"] {
            let mut g = f.clone();
            g[0] = json!(attr);
            assert!(ids(&a, &g).await.is_empty(), "{g}");
        }
    }
    // `id` is a string attribute like any other.
    let want: BTreeSet<String> = (0..10).map(|i| format!("d0000{i}")).collect();
    assert_eq!(ids(&a, &json!(["id", "Glob", "d0000?"])).await, want);
    assert_eq!(
        ids(&a, &json!(["id", "NotGlob", "d*"])).await,
        BTreeSet::new()
    );
    assert_eq!(
        ids(&a, &json!(["id", "NotIGlob", "D*"])).await,
        BTreeSet::new()
    );
}

#[tokio::test]
async fn what_a_pattern_cannot_mean_is_refused() {
    let a = api();
    seeded(&a, 10).await;
    for (f, why) in [
        (json!(["s", "Regex", "("]), "Regex"),
        (json!(["s", "Regex", "\\w{1000}{1000}"]), "Regex"),
        (json!(["s", "Regex", "a".repeat(4097)]), "Regex"),
        (json!(["s", "Glob", "[ab"]), "Glob"),
        (json!(["s", "IGlob", 5]), "IGlob"),
        (
            json!(["s", "Fuzzy", {"value": "a", "max_edits": 3}]),
            "Fuzzy",
        ),
        (
            json!(["s", "Fuzzy", {"value": "a".repeat(257), "max_edits": 1}]),
            "Fuzzy",
        ),
        (json!(["s", "Fuzzy", "a"]), "Fuzzy"),
        (
            json!([
                "Or",
                (0..17)
                    .map(|i| json!(["s", "Glob", format!("{i}*")]))
                    .collect::<Vec<_>>()
            ]),
            "16",
        ),
    ] {
        let q = json!({"rank_by": ["id", "asc"], "filters": f});
        let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f} was accepted: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(why), "{f}: {msg:?} does not name {why:?}");
    }
    // Sixteen is the cap, not past it.
    let sixteen = json!([
        "Or",
        (0..16)
            .map(|i| json!(["s", "Glob", format!("{i}*")]))
            .collect::<Vec<_>>()
    ]);
    ids(&a, &sixteen).await;
}

#[tokio::test]
async fn a_pattern_decides_a_delete_by_filter_as_a_query_does() {
    let a = api();
    let rows = seeded(&a, 300).await;
    let cases = cases();
    for (f, _) in cases.iter().step_by(7) {
        let a = api();
        seeded(&a, 300).await;
        let before = ids(&a, &json!(["And", []])).await;
        let hit = ids(&a, f).await;
        write(&a, json!({"delete_by_filter": f})).await;
        fold(&a).await;
        let after = ids(&a, &json!(["And", []])).await;
        let want: BTreeSet<String> = before.difference(&hit).cloned().collect();
        assert_eq!(after, want, "{f}");
    }
    assert_eq!(rows.len(), 300);
}
