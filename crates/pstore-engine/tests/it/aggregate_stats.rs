//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M232: an aggregation answers from per-block statistics exactly what it answers by reading
//! the rows, for every filter, with deletes and unfolded rows beside them; and it does read
//! fewer bytes when a filter admits blocks whole.

use pstore_blob::{Accounted, MemoryStore, OpClass, TenantView};
use pstore_engine::{Consistency, Engine};
use pstore_format::{BlockOptions, Document, Value};
use pstore_query::{Aggregate, AggregateSpec, Op, Predicate, Total};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(232);

type E = Engine<TenantView<MemoryStore>>;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// `q` an int on every row, `p` a dyadic float on every row (so every sum is exact in either
/// order), `sparse` an int on a third of them, `mixed` an int or a float, `s` a string. `q` and
/// `p` change every 20 and 16 rows, so a 64-row block's zone is narrow and a random literal
/// often lands on its edge, where `must_admit` must be exact. `c` is one value across each
/// 64-row block of a 700-row segment, so an equality can admit a block whole.
fn doc(i: usize) -> Document {
    let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.5]);
    let a = &mut d.attrs;
    a.insert("q".to_owned(), Value::Int((i / 20 % 50) as i64 - 10));
    a.insert(
        "p".to_owned(),
        Value::Float((i / 16 % 40) as f64 / 4.0 - 3.0),
    );
    if i.is_multiple_of(3) {
        a.insert("sparse".to_owned(), Value::Int(i as i64));
    }
    a.insert(
        "mixed".to_owned(),
        if i.is_multiple_of(7) {
            Value::Float(0.5)
        } else {
            Value::Int(1)
        },
    );
    a.insert("s".to_owned(), Value::Str(format!("s{}", i % 4)));
    a.insert("c".to_owned(), Value::Int((i % 700 / 64) as i64));
    d
}

/// Every aggregate over `attrs`. Over only the attributes statistics cover, blocks are
/// answered from them; with any other, every block is read.
fn spec(attrs: &[&str]) -> AggregateSpec {
    let mut labels = vec![("rows".to_owned(), Aggregate::Count(None))];
    for &a in attrs {
        labels.push((format!("count {a}"), Aggregate::Count(Some(a.to_owned()))));
        labels.push((format!("sum {a}"), Aggregate::Sum(a.to_owned())));
        labels.push((format!("min {a}"), Aggregate::Min(a.to_owned())));
        labels.push((format!("max {a}"), Aggregate::Max(a.to_owned())));
        labels.push((format!("avg {a}"), Aggregate::Avg(a.to_owned())));
    }
    AggregateSpec {
        labels,
        group_by: vec![],
        top_k: 10,
    }
}

/// Two engines over two stores, given the same writes, folds and deletes: one sealing
/// aggregate statistics, one not. With `unfolded`, each also holds rows unfolded, whose ids
/// shadow folded rows, so no block is answered from its statistics.
async fn pair(unfolded: bool) -> ((Accounted<MemoryStore>, E), (Accounted<MemoryStore>, E)) {
    let make = |aggregates: bool| {
        let store = Accounted::new(MemoryStore::new());
        let e = Engine::new(Arc::new(store.as_tenant(T)), T, LaneId(1)).with_block_options(
            BlockOptions {
                aggregates,
                ..BlockOptions::default()
            },
        );
        (store, e)
    };
    let (with, without) = (make(true), make(false));
    for (_, e) in [&with, &without] {
        for k in 0..3 {
            e.write("idx", (k * 700..(k + 1) * 700).map(doc).collect())
                .await
                .unwrap();
            e.flush().await.unwrap();
            e.fold().await.unwrap();
        }
        // Deleted rows in two segments, and rows this process holds unfolded.
        e.delete("idx", vec!["d00003".to_owned(), "d01500".to_owned()])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
        if unfolded {
            // New rows, and new versions of folded ones, which hide the old.
            let mut rows: Vec<Document> = (2100..2130).map(doc).collect();
            rows.extend((100..230).map(|i| {
                let mut d = doc(i + 5_000);
                d.id = format!("d{i:05}");
                d
            }));
            e.write("idx", rows).await.unwrap();
        }
    }
    (with, without)
}

const COVERED: &[&str] = &["q", "p", "sparse", "c"];
const EVERY: &[&str] = &["q", "p", "sparse", "c", "mixed", "s"];

async fn totals(e: &E, attrs: &[&str], filter: Option<&Predicate>) -> Vec<Total> {
    let got = e
        .aggregate_as("idx", spec(attrs), filter, None, Consistency::Eventual)
        .await
        .unwrap();
    assert_eq!(got.groups.len(), 1);
    got.groups[0].1.clone()
}

/// The same, grouped by `s`: never answered from statistics.
async fn grouped(e: &E, filter: Option<&Predicate>) -> Vec<(Vec<pstore_query::Key>, Vec<Total>)> {
    let mut spec = spec(COVERED);
    spec.group_by = vec![pstore_query::GroupKey::value("s")];
    e.aggregate_as("idx", spec, filter, None, Consistency::Eventual)
        .await
        .unwrap()
        .groups
}

fn cmp(r: &mut Rng) -> Predicate {
    let name = ["q", "p", "sparse", "c", "mixed", "s"][r.below(6) as usize];
    let op = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte][r.below(5) as usize];
    // Around the attribute's own values, so a literal often lands on a zone's edge.
    let (lo, span) = match name {
        "q" => (-12, 54),
        "sparse" => (-2, 2_104),
        "c" => (-1, 13),
        _ => (-4, 12),
    };
    let want = match r.below(5) {
        0 | 1 => Value::Int(lo + r.below(span) as i64),
        2 => Value::Float(lo as f64 + r.below(span * 4) as f64 / 4.0),
        3 => Value::Int([i64::MIN, i64::MAX][r.below(2) as usize]),
        _ => Value::Str(format!("s{}", r.below(4))),
    };
    Predicate::Cmp(name.to_owned(), op, want)
}

fn predicate(r: &mut Rng, depth: u32) -> Predicate {
    match (depth, r.below(5)) {
        (0, _) | (_, 0 | 1) => cmp(r),
        (_, 2) => Predicate::And((0..2).map(|_| predicate(r, depth - 1)).collect()),
        (_, 3) => Predicate::Or((0..2).map(|_| predicate(r, depth - 1)).collect()),
        _ => Predicate::Not(Box::new(predicate(r, depth - 1))),
    }
}

#[tokio::test]
async fn block_statistics_answer_what_the_rows_do() {
    for unfolded in [false, true] {
        statistics_answer_what_the_rows_do(unfolded).await;
    }
}

async fn statistics_answer_what_the_rows_do(unfolded: bool) {
    let ((_, with), (_, without)) = pair(unfolded).await;
    let fixed = [
        None,
        Some(Predicate::Cmp("q".to_owned(), Op::Gte, Value::Int(-10))),
        Some(Predicate::Cmp("p".to_owned(), Op::Lt, Value::Float(100.0))),
        Some(Predicate::Cmp("sparse".to_owned(), Op::Gte, Value::Int(0))),
        Some(Predicate::And(vec![
            Predicate::Cmp("q".to_owned(), Op::Gte, Value::Int(0)),
            Predicate::Cmp("p".to_owned(), Op::Lt, Value::Float(2.0)),
        ])),
        Some(Predicate::Cmp("c".to_owned(), Op::Eq, Value::Int(3))),
        Some(Predicate::Cmp(
            "id".to_owned(),
            Op::Gte,
            Value::Str("d0".into()),
        )),
    ];
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let random: Vec<Option<Predicate>> = (0..300).map(|_| Some(predicate(&mut r, 2))).collect();
    for f in fixed.iter().chain(&random) {
        for attrs in [COVERED, EVERY, &["q"], &["sparse"]] {
            let a = totals(&with, attrs, f.as_ref()).await;
            let b = totals(&without, attrs, f.as_ref()).await;
            assert_eq!(a, b, "unfolded {unfolded}, {attrs:?}: {f:?}");
        }
        let g = grouped(&with, f.as_ref()).await;
        assert_eq!(g, grouped(&without, f.as_ref()).await, "grouped: {f:?}");
    }
}

#[tokio::test]
async fn a_filter_admitting_blocks_whole_reads_fewer_bytes() {
    let ((ws, with), (os, without)) = pair(false).await;
    let read = |s: &Accounted<MemoryStore>| s.bytes(T, OpClass::Read);
    for f in [
        None,
        Some(Predicate::Cmp("q".to_owned(), Op::Gte, Value::Int(-10))),
    ] {
        let sum = AggregateSpec {
            labels: vec![
                ("sum".to_owned(), Aggregate::Sum("q".to_owned())),
                ("avg".to_owned(), Aggregate::Avg("p".to_owned())),
            ],
            group_by: vec![],
            top_k: 10,
        };
        let (a0, b0) = (read(&ws), read(&os));
        let a = with
            .aggregate_as("idx", sum.clone(), f.as_ref(), None, Consistency::Eventual)
            .await
            .unwrap();
        let b = without
            .aggregate_as("idx", sum, f.as_ref(), None, Consistency::Eventual)
            .await
            .unwrap();
        assert_eq!(a.groups, b.groups);
        let (a1, b1) = (read(&ws) - a0, read(&os) - b0);
        // The block bytes are gone: what is left is the opens and the delete vectors'
        // blocks.
        assert!(
            a1 * 2 < b1,
            "{f:?}: {a1} bytes with statistics, {b1} without"
        );
    }
}
