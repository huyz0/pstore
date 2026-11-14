//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M233: a scan that decodes a columnar block late -- what deciding a row reads first, the rest
//! only for rows admitted -- answers what a row block, decoded whole, answers: every
//! aggregation, an order-by and a scoring expression, under random filters, beside deletes and
//! unfolded rows that shadow folded ones.

use pstore_blob::MemoryStore;
use pstore_engine::{Consistency, Engine};
use pstore_format::{BlockOptions, Document, Value};
use pstore_query::{
    Aggregate, AggregateSpec, Expr, GroupKey, ID_ATTRIBUTE, Op, Order, OrderBy, Predicate,
};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(233);

type E = Engine<MemoryStore>;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// `q` an int, `p` a float, `s` a string on every row; `sparse` on a third; `tag` a long
/// string the filters never read, so only a whole row carries it.
fn doc(i: usize) -> Document {
    let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.5]);
    let a = &mut d.attrs;
    a.insert("q".to_owned(), Value::Int((i * 7 % 41) as i64 - 10));
    a.insert("p".to_owned(), Value::Float((i % 23) as f64 / 4.0 - 2.0));
    a.insert("s".to_owned(), Value::Str(format!("s{}", i % 5)));
    if i.is_multiple_of(3) {
        a.insert("sparse".to_owned(), Value::Int(i as i64));
    }
    a.insert(
        "tag".to_owned(),
        Value::Str(format!("tag-{i}-{}", "x".repeat(i % 9))),
    );
    d
}

/// Two engines over two stores, given the same writes, deletes and unfolded rows: one sealing
/// columnar blocks, one row blocks. Neither seals aggregate statistics, so every block is read.
async fn pair() -> (E, E) {
    let make = |columnar: bool| {
        Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1)).with_block_options(BlockOptions {
            columnar,
            equality: true,
            aggregates: false,
        })
    };
    let (late, whole) = (make(true), make(false));
    for e in [&late, &whole] {
        for k in 0..3 {
            e.write("idx", (k * 500..(k + 1) * 500).map(doc).collect())
                .await
                .unwrap();
            e.flush().await.unwrap();
            e.fold().await.unwrap();
        }
        e.delete("idx", vec!["d00007".to_owned(), "d00777".to_owned()])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
        // New rows, and new versions of folded ones, which hide the old.
        let mut rows: Vec<Document> = (1500..1520).map(doc).collect();
        rows.extend((40..90).map(|i| {
            let mut d = doc(i + 3_000);
            d.id = format!("d{i:05}");
            d
        }));
        e.write("idx", rows).await.unwrap();
    }
    (late, whole)
}

fn cmp(r: &mut Rng) -> Predicate {
    let name = ["q", "p", "s", "sparse", ID_ATTRIBUTE][r.below(5) as usize];
    let op = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte][r.below(5) as usize];
    let want = match (name, r.below(3)) {
        (ID_ATTRIBUTE, _) => Value::Str(format!("d{:05}", r.below(1_600))),
        ("s", _) | (_, 2) => Value::Str(format!("s{}", r.below(5))),
        (_, 0) => Value::Int(r.below(60) as i64 - 20),
        _ => Value::Float(r.below(40) as f64 / 4.0 - 4.0),
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

fn spec(grouped: bool) -> AggregateSpec {
    let mut labels = vec![("rows".to_owned(), Aggregate::Count(None))];
    for a in ["q", "p", "sparse"] {
        labels.push((format!("count {a}"), Aggregate::Count(Some(a.to_owned()))));
        labels.push((format!("sum {a}"), Aggregate::Sum(a.to_owned())));
        labels.push((format!("min {a}"), Aggregate::Min(a.to_owned())));
        labels.push((format!("max {a}"), Aggregate::Max(a.to_owned())));
    }
    AggregateSpec {
        labels,
        group_by: if grouped {
            vec![GroupKey::value("s")]
        } else {
            vec![]
        },
        top_k: 10,
    }
}

#[tokio::test]
async fn late_materialization_answers_as_whole_rows_do() {
    let (late, whole) = pair().await;
    let order = Order::new(vec![
        OrderBy {
            attr: "s".to_owned(),
            desc: true,
        },
        OrderBy {
            attr: "q".to_owned(),
            desc: false,
        },
    ])
    .unwrap();
    let expr = Expr::Max(vec![
        Expr::Literal(0.0),
        Expr::Sum(vec![
            Expr::Attribute("q".to_owned()),
            Expr::Product(0.5, Box::new(Expr::Attribute("p".to_owned()))),
        ]),
    ]);
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let filters: Vec<Option<Predicate>> = std::iter::once(None)
        .chain((0..200).map(|_| Some(predicate(&mut r, 2))))
        .collect();
    let ev = Consistency::Eventual;
    for f in &filters {
        let f = f.as_ref();
        for grouped in [false, true] {
            let a = late.aggregate_as("idx", spec(grouped), f, None, ev).await;
            let b = whole.aggregate_as("idx", spec(grouped), f, None, ev).await;
            assert_eq!(
                a.unwrap().groups,
                b.unwrap().groups,
                "grouped {grouped}: {f:?}"
            );
        }
        for (offset, limit) in [(0, 7), (5, 30)] {
            let a = late
                .ordered_as("idx", order.clone(), f, offset, limit, None, ev)
                .await
                .unwrap();
            let b = whole
                .ordered_as("idx", order.clone(), f, offset, limit, None, ev)
                .await
                .unwrap();
            assert_eq!(a.rows, b.rows, "order {offset}+{limit}: {f:?}");
            assert_eq!(a.unfolded, b.unfolded);
        }
        let a = late
            .scored_as("idx", &expr, f, 0, 9, None, ev)
            .await
            .unwrap();
        let b = whole
            .scored_as("idx", &expr, f, 0, 9, None, ev)
            .await
            .unwrap();
        assert_eq!(a.ordered.rows, b.ordered.rows, "scored: {f:?}");
        assert_eq!(a.scores, b.scores, "scored: {f:?}");
    }
}
