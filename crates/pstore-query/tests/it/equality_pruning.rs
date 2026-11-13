//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! String and bool equality prunes blocks (M230): a block whose equality section proves no
//! row holds the literal is not read, and no block holding a match is ever dropped.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Segment, SegmentWriter, Value};
use pstore_query::{Op, Predicate};

async fn open(docs: &[Document], rows_per_block: usize) -> Segment {
    let mut w = SegmentWriter::new(rows_per_block);
    for d in docs {
        w.push(d.clone());
    }
    let s = MemoryStore::new();
    let key = Key::new("seg/eq");
    s.put(&key, w.try_finish().unwrap()).await.unwrap();
    Segment::open(&s, &key).await.unwrap()
}

fn eq(name: &str, v: Value) -> Predicate {
    Predicate::Cmp(name.to_owned(), Op::Eq, v)
}

/// The blocks a predicate keeps, by index.
fn kept(seg: &Segment, p: &Predicate) -> Vec<usize> {
    seg.block_zones()
        .enumerate()
        .filter(|(_, z)| p.could_admit(z))
        .map(|(i, _)| i)
        .collect()
}

/// 8 blocks of 10 rows: block `b` holds customer `c{b}` only, and `archived` is true in
/// blocks 0 and 1 only.
fn clustered() -> Vec<Document> {
    (0..80)
        .map(|i| {
            let mut d = Document::new(format!("d{i:03}"), vec![1.0, 0.0]);
            d.attrs
                .insert("customer".into(), Value::Str(format!("c{}", i / 10)));
            d.attrs.insert("archived".into(), Value::Bool(i < 20));
            d
        })
        .collect()
}

#[tokio::test]
async fn equality_prunes_blocks_without_the_literal() {
    let docs = clustered();
    let seg = open(&docs, 10).await;
    assert_eq!(seg.block_count(), 8);
    let s = |v: &str| Value::Str(v.to_owned());
    assert_eq!(kept(&seg, &eq("customer", s("c3"))), vec![3]);
    assert_eq!(
        kept(&seg, &eq("customer", s("nobody"))),
        Vec::<usize>::new()
    );
    assert_eq!(
        kept(
            &seg,
            &Predicate::In("customer".into(), vec![s("c1"), s("c6"), s("zz")])
        ),
        vec![1, 6]
    );
    assert_eq!(kept(&seg, &eq("archived", Value::Bool(true))), vec![0, 1]);
    assert_eq!(
        kept(&seg, &eq("archived", Value::Bool(false))),
        vec![2, 3, 4, 5, 6, 7]
    );
    // Composed: `And` takes the narrower, `Or` the union, `Not` prunes nothing.
    let both = Predicate::And(vec![
        eq("customer", s("c1")),
        eq("archived", Value::Bool(false)),
    ]);
    assert_eq!(kept(&seg, &both), Vec::<usize>::new());
    let either = Predicate::Or(vec![eq("customer", s("c5")), eq("customer", s("c7"))]);
    assert_eq!(kept(&seg, &either), vec![5, 7]);
    assert_eq!(
        kept(&seg, &Predicate::Not(Box::new(eq("customer", s("c3"))))).len(),
        8
    );
    // Every pruned block holds no match; every kept one holds one.
    for (p, want) in [
        (eq("customer", s("c3")), 10),
        (eq("archived", Value::Bool(true)), 20),
    ] {
        let rows = docs.iter().filter(|d| p.admits(&d.id, &d.attrs)).count();
        assert_eq!(rows, want);
    }
}

/// A tiny deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A value of any kind an attribute holds, from small pools so literals often match.
fn value(r: &mut Rng) -> Value {
    match r.below(9) {
        0 | 1 => Value::Str(format!("v{}", r.below(6))),
        2 => Value::Bool(r.below(2) == 0),
        // A datetime whose RFC 3339 form a string literal below spells.
        3 => Value::DateTime(1_700_000_000_000_000 + r.below(3) as i64 * 1_000_000),
        4 => Value::Uuid([r.below(3) as u8; 16]),
        5 => Value::Bytes(vec![r.below(3) as u8; 2]),
        6 => Value::Int(r.below(4) as i64),
        7 => Value::Array(vec![Value::Str(format!("v{}", r.below(6)))]),
        _ => Value::Float(r.below(4) as f64),
    }
}

/// A literal of any kind a filter names, including string spellings of the datetime, uuid
/// and bytes values above.
fn literal(r: &mut Rng) -> Value {
    match r.below(7) {
        0 | 1 => Value::Str(format!("v{}", r.below(7))),
        2 => Value::Bool(r.below(2) == 0),
        3 => Value::Str(format!("2023-11-14T22:13:2{}Z", r.below(3))),
        4 => {
            let b = r.below(3);
            Value::Str(format!(
                "0{b}0{b}0{b}0{b}-0{b}0{b}-0{b}0{b}-0{b}0{b}-0{b}0{b}0{b}0{b}0{b}0{b}"
            ))
        }
        5 => Value::Uuid([r.below(3) as u8; 16]),
        _ => Value::Int(r.below(4) as i64),
    }
}

fn leaf(r: &mut Rng) -> Predicate {
    let name = if r.below(2) == 0 { "a" } else { "b" };
    match r.below(5) {
        0 => Predicate::In(name.into(), (0..3).map(|_| literal(r)).collect()),
        // An order comparison of a string or a bool: the section must leave it alone.
        1 => Predicate::Cmp(name.into(), Op::Lt, literal(r)),
        2 => Predicate::Cmp(name.into(), Op::Gte, literal(r)),
        _ => eq(name, literal(r)),
    }
}

fn predicate(r: &mut Rng, depth: u32) -> Predicate {
    if depth == 0 {
        return leaf(r);
    }
    match r.below(5) {
        0 => Predicate::And((0..2).map(|_| predicate(r, depth - 1)).collect()),
        1 => Predicate::Or((0..2).map(|_| predicate(r, depth - 1)).collect()),
        2 => Predicate::Not(Box::new(predicate(r, depth - 1))),
        _ => leaf(r),
    }
}

#[tokio::test]
async fn equality_pruning_never_drops_a_matching_block() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut pruned = 0usize;
    for _ in 0..60 {
        // Blocks of 4: some hold only strings, some only bools, some mixed with other kinds.
        let docs: Vec<Document> = (0..48)
            .map(|i| {
                let mut d = Document::new(format!("d{i:03}"), vec![1.0, 0.0]);
                for name in ["a", "b"] {
                    if r.below(5) != 0 {
                        // Half the blocks keep to strings and bools, so the section is there.
                        let v = if (i / 4) % 2 == 0 {
                            match r.below(3) {
                                0 => Value::Bool(r.below(2) == 0),
                                _ => Value::Str(format!("v{}", r.below(6))),
                            }
                        } else {
                            value(&mut r)
                        };
                        d.attrs.insert(name.into(), v);
                    }
                }
                d
            })
            .collect();
        let seg = open(&docs, 4).await;
        let blocks: Vec<&[Document]> = docs.chunks(4).collect();
        assert_eq!(seg.block_count(), blocks.len());
        for _ in 0..80 {
            let p = predicate(&mut r, 2);
            for (b, z) in seg.block_zones().enumerate() {
                let matches = blocks[b].iter().any(|d| p.admits(&d.id, &d.attrs));
                let admit = p.could_admit(z);
                assert!(!matches || admit, "block {b} pruned under {p:?}");
                // Counted only where nothing but the section can prune.
                let by_section = matches!(
                    &p,
                    Predicate::Cmp(_, Op::Eq, Value::Str(_) | Value::Bool(_))
                );
                pruned += usize::from(!admit && by_section);
            }
        }
    }
    // Not vacuous: the section does prune. Without it, the zone maps alone pruned 326 of
    // these (datetime-only blocks against a string that is not RFC 3339); with it, 7,017.
    assert!(pruned > 3000, "only {pruned} blocks pruned");
}
