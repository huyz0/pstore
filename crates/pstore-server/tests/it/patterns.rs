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
    Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
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

/// M145's reference, written here: some substring of some string element within `k` edits
/// of `q`, every start and end tried, lowercased when case-insensitive (the rows below are
/// ASCII, `é` and `É`, where lowercasing and the index's fold agree).
fn fuzzy_substring(q: &str, k: usize, case: bool, v: &Value) -> bool {
    let key = |s: &str| -> Vec<char> {
        s.chars()
            .map(|c| {
                if case {
                    c
                } else {
                    c.to_lowercase().next().unwrap()
                }
            })
            .collect()
    };
    let q = key(q);
    let hit = |s: &str| {
        let s = key(s);
        (0..=s.len()).any(|a| (a..=s.len()).any(|b| levenshtein(&s[a..b], &q) <= k))
    };
    match v {
        Value::String(s) => hit(s),
        Value::Array(items) => items.iter().any(|e| e.as_str().is_some_and(hit)),
        _ => false,
    }
}

/// The vendor's options: `[{3, 0}, {6, 1}]` unless given.
fn vendor(attr: &str, q: &str, steps: &[(u64, u64)], case: Option<bool>) -> Value {
    let steps: Vec<Value> = steps
        .iter()
        .map(|(m, d)| json!({"min_query_chars": m, "distance": d}))
        .collect();
    let mut o = json!({"max_edit_distance": steps});
    if let Some(c) = case {
        o["case_sensitive"] = json!(c);
    }
    json!([attr, "Fuzzy", q, o])
}

const STEPS: &[(u64, u64)] = &[(3, 0), (6, 1)];

#[tokio::test]
async fn the_vendor_fuzzy_picks_its_distance_by_query_length() {
    // M145 criterion 2, before and after the fold.
    let a = api();
    let docs: Vec<Value> = [
        ("t1", json!("turbopuffer inc")),
        ("t2", json!("turbo")),
        ("t3", json!("zzz")),
        ("t4", json!(["x", "turbo"])),
    ]
    .into_iter()
    .map(|(id, v)| json!({"id": id, "vector": [1.0, 0.5], "attributes": {"s": v}}))
    .collect();
    write(&a, json!({"documents": docs})).await;
    let all: BTreeSet<String> = ["t1", "t2", "t3", "t4"].map(String::from).into();
    let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        let at = format!("folded: {folded}");
        assert_eq!(
            ids(&a, &vendor("s", "turbopufer", STEPS, None)).await,
            set(&["t1"]),
            "{at}"
        );
        // Five characters take `k = 0`, six take `k = 1`: a substitution, then an insertion.
        assert_eq!(
            ids(&a, &vendor("s", "turxo", STEPS, None)).await,
            set(&[]),
            "{at}"
        );
        assert_eq!(
            ids(&a, &vendor("s", "turbxo", STEPS, None)).await,
            set(&["t1", "t2", "t4"]),
            "{at}"
        );
        assert_eq!(
            ids(&a, &vendor("s", "tu", STEPS, None)).await,
            set(&[]),
            "{at}"
        );
        assert_eq!(
            ids(&a, &json!(["Not", vendor("s", "tu", STEPS, None)])).await,
            all,
            "{at}"
        );
        assert_eq!(
            ids(&a, &vendor("s", "TURBO", STEPS, Some(false))).await,
            set(&["t1", "t2", "t4"]),
            "{at}"
        );
        assert_eq!(
            ids(&a, &vendor("s", "TURBO", STEPS, None)).await,
            set(&[]),
            "{at}"
        );
        assert_eq!(
            ids(&a, &vendor("s", "TURBO", STEPS, Some(true))).await,
            set(&[]),
            "{at}"
        );
    }
}

#[tokio::test]
async fn the_vendor_fuzzy_reads_arrays_and_decides_a_delete_by_filter() {
    // M145 criterion 3: the reference above decides every answer.
    let rows = [
        ("a", json!("aAé")),
        ("b", json!(["", "éaÉa"])),
        ("c", json!([5])),
        ("d", json!([])),
        ("e", json!(5)),
        ("f", json!("AAAA")),
        ("g", json!(["zz", 5])),
        ("h", json!("xaÉAx")),
        ("i", json!(["q", "xxaaaéab"])),
    ];
    let filters = [
        ("éaé", 0, false),
        ("éaé", 0, true),
        ("aaaa", 0, false),
        ("aaaa", 1, true),
        ("zzq", 1, true),
        ("aÉa", 0, true),
        ("aaaéaa", 1, false),
        ("aaaéaa", 1, true),
        ("aaaéaaéaa", 2, false),
    ];
    for (q, k, case) in filters {
        // One step: `k` from `3 * (k + 1)` characters, and below that nothing matches.
        let steps = [(3 * (k + 1), k)];
        let f = vendor("s", q, &steps, Some(case));
        let long = q.chars().count() as u64 >= 3 * (k + 1);
        let want: BTreeSet<String> = rows
            .iter()
            .filter(|(_, v)| long && fuzzy_substring(q, k as usize, case, v))
            .map(|(id, _)| (*id).to_owned())
            .collect();
        for folded in [false, true] {
            let a = api();
            let docs: Vec<Value> = rows
                .iter()
                .map(|(id, v)| json!({"id": id, "vector": [1.0, 0.5], "attributes": {"s": v}}))
                .collect();
            write(&a, json!({"documents": docs})).await;
            if folded {
                fold(&a).await;
            }
            assert_eq!(ids(&a, &f).await, want, "{f} folded: {folded}");
            let before = ids(&a, &json!(["And", []])).await;
            write(&a, json!({"delete_by_filter": f})).await;
            fold(&a).await;
            let after = ids(&a, &json!(["And", []])).await;
            let kept: BTreeSet<String> = before.difference(&want).cloned().collect();
            assert_eq!(after, kept, "delete {f} folded: {folded}");
        }
    }
}

#[tokio::test]
async fn what_the_vendor_fuzzy_cannot_mean_is_refused() {
    // M145 criterion 4.
    let a = api();
    seeded(&a, 10).await;
    let step =
        |m: Value, d: Value| json!({"max_edit_distance": [{"min_query_chars": m, "distance": d}]});
    let ok = step(json!(3), json!(0));
    for f in [
        json!(["s", "Fuzzy", 5, ok]),
        json!(["s", "Fuzzy", "a".repeat(257), ok]),
        json!(["s", "Fuzzy", "abc", null]),
        json!(["s", "Fuzzy", "abc", {}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": [{"min_query_chars": 3, "distance": 0}], "x": 1}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": [{"min_query_chars": 3, "distance": 0}], "case_sensitive": 1}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": 1}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": []}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": (0..9).map(|i| json!({"min_query_chars": 3 + i, "distance": 0})).collect::<Vec<_>>()}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": [5]}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": [{"min_query_chars": 3}]}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": [{"min_query_chars": 3, "distance": 0, "x": 1}]}]),
        json!(["s", "Fuzzy", "abc", step(json!(-3), json!(0))]),
        json!(["s", "Fuzzy", "abc", step(json!(3), json!(-1))]),
        json!(["s", "Fuzzy", "abc", step(json!(3.5), json!(0))]),
        json!(["s", "Fuzzy", "abc", step(json!(12), json!(3))]),
        json!(["s", "Fuzzy", "abc", step(json!(5), json!(1))]),
        json!(["s", "Fuzzy", "abc", step(json!(8), json!(2))]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": [{"min_query_chars": 6, "distance": 1}, {"min_query_chars": 6, "distance": 1}]}]),
        json!(["s", "Fuzzy", "abc", {"max_edit_distance": [{"min_query_chars": 6, "distance": 1}, {"min_query_chars": 3, "distance": 0}]}]),
        json!(["s", "Fuzzy", "abc", ok, {}]),
        json!(["$id", "Fuzzy", "abc", ok]),
        json!(["", "Fuzzy", "abc", ok]),
        json!(["s", "Fuzzy", "abc"]),
    ] {
        let q = json!({"rank_by": ["id", "asc"], "filters": f});
        let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f} was accepted: {b}");
    }
    // The M15 shape answers as before, and the vendor's `false` and an omitted flag are the
    // two settings, not refusals.
    ids(&a, &json!(["s", "Fuzzy", {"value": "abc", "max_edits": 1}])).await;
    ids(&a, &vendor("s", "abc", &[(3, 0)], Some(false))).await;
    ids(
        &a,
        &vendor("s", "abcdefghi", &[(3, 0), (6, 1), (9, 2)], None),
    )
    .await;
}
