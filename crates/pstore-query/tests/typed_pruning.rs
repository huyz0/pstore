//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Pruning over ints and floats together is sound (M9h.1): whatever zone maps a segment
//! carries, the rows `could_admit` lets through hold every row `admits` accepts.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Segment, SegmentWriter, Value};
use pstore_query::{Op, Predicate};
use std::collections::BTreeSet;
use std::sync::Mutex;

const BIG: i64 = 9_007_199_254_740_993; // 2^53 + 1

fn values(mixed: bool) -> Vec<Value> {
    let mut v = vec![
        Value::Int(0),
        Value::Int(-3),
        Value::Int(7),
        Value::Int(BIG),
        Value::Int(1),
        Value::Int(2),
        Value::Int(40),
        Value::Int(-100),
    ];
    if mixed {
        v.extend([
            Value::Float(-0.0),
            Value::Float(0.0),
            Value::Float(2.5),
            Value::Float(9_007_199_254_740_992.0),
            Value::Float(-3.5),
            Value::Float(40.0),
            Value::Float(1e10),
            Value::Float(0.25),
        ]);
    }
    v
}

/// Rows in blocks of 3, so every block mixes whatever types the values do. Every fourth row
/// lacks `n`, so a zone never covers a whole block.
fn docs(mixed: bool) -> Vec<Document> {
    let mut out = Vec::new();
    for (i, v) in values(mixed).into_iter().enumerate() {
        let mut d = Document::new(format!("d{i:03}"), vec![1.0, 0.0]);
        d.attrs.insert("n".to_owned(), v);
        out.push(d);
        if i % 4 == 3 {
            out.push(Document::new(format!("e{i:03}"), vec![0.0, 1.0]));
        }
    }
    if mixed {
        // M9h.2 (spec review, m6): a block whose `n` is only arrays, and a block where an
        // array holds a number outside that block's zones. Padded so each is one block of 3.
        while out.len() % 3 != 0 {
            out.push(Document::new(format!("pad{}", out.len()), vec![0.0, 1.0]));
        }
        let arrays = [
            Value::Array(vec![Value::Int(1), Value::Str("2".to_owned())]),
            Value::Array(vec![Value::Float(2.5)]),
            Value::Array(vec![]),
            Value::Int(5),
            Value::Array(vec![Value::Float(1e12), Value::Int(BIG), Value::Bool(true)]),
            Value::Float(6.0),
        ];
        for (i, v) in arrays.into_iter().enumerate() {
            let mut d = Document::new(format!("a{i:03}"), vec![1.0, 1.0]);
            d.attrs.insert("n".to_owned(), v);
            out.push(d);
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

fn literals() -> Vec<Value> {
    let mut l = values(true);
    l.extend([
        Value::Int(-1000),
        Value::Int(1_000_000),
        Value::Float(1.5),
        Value::Float(-0.5),
        Value::Float(1e11),
        Value::Int(BIG - 1),
        Value::Str("2".to_owned()),
        Value::Bool(true),
    ]);
    l
}

fn predicates() -> Vec<Predicate> {
    let n = || "n".to_owned();
    let mut out = Vec::new();
    for x in literals() {
        for op in [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte] {
            let p = Predicate::Cmp(n(), op, x.clone());
            out.push(Predicate::Not(Box::new(p.clone())));
            out.push(p);
        }
    }
    let l = literals();
    for pair in l.windows(2) {
        out.push(Predicate::In(n(), pair.to_vec()));
    }
    for pair in l.windows(2) {
        let p = Predicate::ContainsAny(n(), pair.to_vec());
        out.push(Predicate::Not(Box::new(p.clone())));
        out.push(p);
    }
    for x in [Value::Float(1e12), Value::Int(1), Value::Float(1.0)] {
        out.push(Predicate::ContainsAny(n(), vec![x]));
    }
    out.push(Predicate::ContainsAny(n(), vec![]));
    out.push(Predicate::In(n(), vec![Value::Float(-0.0)]));
    out.push(Predicate::In(n(), vec![Value::Int(0)]));
    out
}

/// Every predicate admits the same rows through the zones as by brute force; returns
/// whether the segment's blocks carried any zone.
async fn sound(docs: &[Document], bytes: bytes::Bytes) -> bool {
    let store = MemoryStore::new();
    let key = Key::new("seg");
    store.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&store, &key).await.unwrap();
    let zoned = Mutex::new(false);
    for p in predicates() {
        let want: BTreeSet<String> = docs
            .iter()
            .filter(|d| p.admits(&d.id, &d.attrs))
            .map(|d| d.id.clone())
            .collect();
        let got: BTreeSet<String> = seg
            .rows_where(&store, &key, |z| {
                if !z.ints.is_empty() || !z.floats.is_empty() {
                    *zoned.lock().unwrap() = true;
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
    zoned.into_inner().unwrap()
}

#[tokio::test]
async fn mixed_ints_and_floats_prune_soundly_with_zone_maps() {
    let docs = docs(true);
    assert!(sound(&docs, written(&docs, None).unwrap()).await);
}

#[tokio::test]
async fn an_int_only_segment_prunes_soundly() {
    let docs = docs(false);
    assert!(sound(&docs, written(&docs, None).unwrap()).await);
}

#[tokio::test]
async fn a_zone_free_typed_segment_prunes_soundly() {
    // The largest budget the zone-mapped index does not fit and the zone-free one does:
    // below it the segment is refused, above it zones return.
    let docs = docs(true);
    let full = written(&docs, None).unwrap().len();
    let mut found = None;
    for budget in (0..full).rev() {
        if let Some(b) = written(&docs, Some(budget))
            && !has_zones(b.clone()).await
        {
            found = Some(b);
            break;
        }
    }
    let bytes = found.expect("some budget seals without zones");
    assert!(!sound(&docs, bytes).await, "the fallback kept zones");
}

async fn has_zones(bytes: bytes::Bytes) -> bool {
    let store = MemoryStore::new();
    let key = Key::new("seg");
    store.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&store, &key).await.unwrap();
    let zoned = Mutex::new(false);
    seg.rows_where(&store, &key, |z| {
        if !z.ints.is_empty() || !z.floats.is_empty() || z.complete {
            *zoned.lock().unwrap() = true;
        }
        true
    })
    .await
    .unwrap();
    zoned.into_inner().unwrap()
}
