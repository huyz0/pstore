//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Pruning with datetimes is sound (M9h.3): a string literal meets a datetime by instant and a
//! string by bytes, and whatever zones a segment carries, the rows `could_admit` lets through
//! hold every row `admits` accepts.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Segment, SegmentWriter, Value, datetime};
use pstore_query::{ID_ATTRIBUTE, Op, Predicate};
use std::collections::BTreeSet;
use std::sync::Mutex;

fn dt(s: &str) -> Value {
    Value::DateTime(datetime::parse(s).unwrap())
}

fn s(x: &str) -> Value {
    Value::Str(x.to_owned())
}

/// Blocks of 3, each row's id itself RFC 3339 (spec review, B1). `None` rows lack `t`.
fn blocks(dates: bool) -> Vec<[Option<Value>; 3]> {
    let mut out = vec![
        // Strings and ints alone: some date-like, some not.
        [
            Some(s("2024-03-01T00:00:00Z")),
            Some(Value::Int(5)),
            Some(s("apple")),
        ],
    ];
    if dates {
        out.extend([
            // Datetimes and no string.
            [
                Some(dt("2024-01-01T00:00:00Z")),
                Some(dt("2024-01-02T00:00:00Z")),
                None,
            ],
            // Only arrays of datetimes.
            [
                Some(Value::Array(vec![dt("2030-01-01T00:00:00Z")])),
                Some(Value::Array(vec![dt("2020-01-01T00:00:00Z"), s("x")])),
                Some(Value::Array(vec![])),
            ],
            // An array element outside the scalar datetime zone.
            [
                Some(dt("2024-06-01T00:00:00Z")),
                Some(Value::Array(vec![dt("2031-01-01T00:00:00Z")])),
                Some(Value::Int(7)),
            ],
            // A date-like string outside the datetime zone.
            [
                Some(dt("2024-06-01T00:00:00Z")),
                Some(s("2029-01-01T00:00:00Z")),
                Some(s("zzz")),
            ],
            // A datetime at the microsecond neighbours.
            [
                Some(dt("2024-06-01T00:00:00.000001Z")),
                Some(dt("2024-05-31T23:59:59.999999Z")),
                Some(Value::Float(1.5)),
            ],
        ]);
    }
    out
}

fn docs(dates: bool) -> Vec<Document> {
    let mut out = Vec::new();
    for (b, rows) in blocks(dates).into_iter().enumerate() {
        for (r, v) in rows.into_iter().enumerate() {
            let id = format!("2024-0{}-0{}T00:00:00Z", b + 1, r + 1);
            let mut d = Document::new(id, vec![1.0, 0.0]);
            if let Some(v) = v {
                d.attrs.insert("t".to_owned(), v);
            }
            // An ATTRIBUTE named `id`, which only the engine API can write: its datetime zone
            // describes it, never the document id a filter on `id` reads.
            if dates && b == 1 {
                d.attrs
                    .insert(ID_ATTRIBUTE.to_owned(), dt("2000-01-01T00:00:00Z"));
            }
            out.push(d);
        }
    }
    out
}

fn literals() -> Vec<Value> {
    [
        "2024-01-01T00:00:00Z",
        "2024-01-01T00:00:00.000001Z",
        "2023-12-31T23:59:59.999999Z",
        "2024-01-02T00:00:00Z",
        "2024-06-01T00:00:00Z",
        "2024-06-01T02:00:00+02:00",
        "2024-05-31T23:59:59.999999Z",
        "2024-06-01T00:00:00.000001Z",
        "2024-06-01T00:00:00.000002Z",
        "2029-01-01T00:00:00Z",
        "2030-01-01T00:00:00Z",
        "2031-01-01T00:00:00Z",
        "2024-03-01T00:00:00Z",
        "2024-02-01T00:00:00Z",
        "2024-01-01T00:00:00+01:00",
        "apple",
        "zzz",
        "not a date",
        "",
    ]
    .into_iter()
    .map(s)
    .chain([Value::Int(5), Value::Float(1.5)])
    .collect()
}

fn predicates() -> Vec<Predicate> {
    let mut out = Vec::new();
    for name in ["t", ID_ATTRIBUTE] {
        let n = || name.to_owned();
        for x in literals() {
            for op in [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte] {
                let p = Predicate::Cmp(n(), op, x.clone());
                out.push(Predicate::Not(Box::new(p.clone())));
                out.push(p);
            }
        }
        for pair in literals().windows(2) {
            out.push(Predicate::In(n(), pair.to_vec()));
            let c = Predicate::ContainsAny(n(), pair.to_vec());
            out.push(Predicate::Not(Box::new(c.clone())));
            out.push(c);
        }
    }
    out
}

fn written(docs: &[Document], budget: Option<usize>) -> Option<bytes::Bytes> {
    let mut w = SegmentWriter::new(3);
    if let Some(b) = budget {
        w = w.with_index_budget(b);
    }
    for d in docs {
        w.push(d.clone());
    }
    w.try_finish().ok()
}

async fn open(bytes: bytes::Bytes) -> (MemoryStore, Key, Segment) {
    let store = MemoryStore::new();
    let key = Key::new("seg");
    store.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&store, &key).await.unwrap();
    (store, key, seg)
}

/// Every predicate admits the same rows through the zones as by brute force; returns whether
/// any block carried a datetime zone.
async fn sound(docs: &[Document], bytes: bytes::Bytes) -> bool {
    let (store, key, seg) = open(bytes).await;
    let dated = Mutex::new(false);
    for p in predicates() {
        let want: BTreeSet<String> = docs
            .iter()
            .filter(|d| p.admits(&d.id, &d.attrs))
            .map(|d| d.id.clone())
            .collect();
        let got: BTreeSet<String> = seg
            .rows_where(&store, &key, |z| {
                if !z.datetimes.is_empty() {
                    *dated.lock().unwrap() = true;
                }
                p.could_admit(z)
            })
            .await
            .unwrap()
            .into_iter()
            .filter(|(_, d)| p.admits(&d.id, &d.attrs))
            .map(|(_, d)| d.id)
            .collect();
        assert_eq!(got, want, "{p:?}");
    }
    dated.into_inner().unwrap()
}

async fn zoned(bytes: bytes::Bytes) -> bool {
    let (store, key, seg) = open(bytes).await;
    let any = Mutex::new(false);
    seg.rows_where(&store, &key, |z| {
        if z.complete || !z.ints.is_empty() || !z.datetimes.is_empty() {
            *any.lock().unwrap() = true;
        }
        true
    })
    .await
    .unwrap();
    any.into_inner().unwrap()
}

#[tokio::test]
async fn a_segment_with_datetime_zones_prunes_soundly() {
    let docs = docs(true);
    assert!(sound(&docs, written(&docs, None).unwrap()).await);
}

#[tokio::test]
async fn a_zone_free_datetime_segment_prunes_soundly() {
    let docs = docs(true);
    let full = written(&docs, None).unwrap().len();
    let mut found = None;
    for budget in (0..full).rev() {
        if let Some(b) = written(&docs, Some(budget))
            && !zoned(b.clone()).await
        {
            found = Some(b);
            break;
        }
    }
    let bytes = found.expect("some budget seals without zones");
    assert!(!sound(&docs, bytes).await);
}

#[tokio::test]
async fn a_typed_segment_without_datetimes_prunes_soundly() {
    // Flag 1: the same rows with no datetime, scalar or in an array, and no `id` attribute;
    // a float keeps it typed.
    let docs: Vec<Document> = docs(true)
        .into_iter()
        .filter(|d| !matches!(d.attrs.get("t"), Some(Value::DateTime(_) | Value::Array(_))))
        .map(|mut d| {
            d.attrs.remove(ID_ATTRIBUTE);
            d
        })
        .collect();
    assert!(!sound(&docs, written(&docs, None).unwrap()).await);
}

#[tokio::test]
async fn an_untyped_segment_of_strings_and_ints_prunes_soundly() {
    let docs = docs(false);
    assert!(!sound(&docs, written(&docs, None).unwrap()).await);
}
