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

//! Columnar blocks (M231): a v3 segment answers every read exactly as the same documents' row
//! blocks do, and refuses a malformed block rather than misreading it.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::columnar::{decode_block, encode_block};
use pstore_format::{Document, Projection, Segment, SegmentWriter, Value, Zones};

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

/// A scalar of any kind, from pools wide and narrow, so columns land on every codec.
fn scalar(r: &mut Rng, pool: u64) -> Value {
    match r.below(12) {
        0 => Value::Int(r.below(pool) as i64 - 3),
        1 => Value::Int([i64::MIN, i64::MAX, 0, -1][r.below(4) as usize]),
        2 => Value::Str(format!("s{}", r.below(pool))),
        3 => Value::Str(String::new()),
        4 => Value::Float([0.5, -0.0, 0.1, 1e300, 2.25, f64::MIN_POSITIVE][r.below(6) as usize]),
        5 => Value::Float(r.below(pool) as f64 / 100.0),
        6 => Value::Bool(r.below(2) == 0),
        7 => Value::DateTime(1_700_000_000_000_000 + r.below(pool) as i64 * 1_000_000),
        8 => Value::Uuid([r.below(pool) as u8; 16]),
        9 => Value::Uint([u64::MAX, 1 << 63, r.below(pool)][r.below(3) as usize]),
        10 => Value::Bytes(vec![r.below(256) as u8; r.below(4) as usize]),
        _ => Value::Array(vec![
            Value::Int(r.below(pool) as i64),
            Value::Null,
            Value::Str("a".into()),
        ]),
    }
}

/// Documents whose attributes are, per name, one kind for every row (a typed column), one
/// kind with gaps, or mixed kinds.
fn docs(r: &mut Rng, n: usize) -> Vec<Document> {
    // Per name: the kind's seed, and how its rows vary.
    let shapes: Vec<(u64, u64)> = (0..6).map(|_| (r.next(), r.below(3))).collect();
    (0..n)
        .map(|i| {
            let mut d = Document::new(format!("id-{}", r.below(1000)), vec![1.0, 0.0]);
            for (a, (seed, shape)) in shapes.iter().enumerate() {
                let pool = [3, 1000][a % 2];
                let v = match shape {
                    // One kind for every row: a generator fixed by the name's seed.
                    0 => scalar(&mut Rng(seed ^ ((i as u64 + 1) * 0x9e37_79b9)), pool),
                    1 if r.below(3) == 0 => continue,
                    1 => scalar(&mut Rng(seed ^ ((i as u64 + 1) * 0x9e37_79b9)), pool),
                    _ => scalar(r, pool),
                };
                d.attrs.insert(format!("a{a}"), v);
            }
            if i % 5 == 0 {
                d.attrs.insert("sorted".into(), Value::Int(i as i64 * 7));
            }
            d
        })
        .collect()
}

fn same(a: &[(usize, Document)], b: &[(usize, Document)]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|((i, x), (j, y))| {
            i == j
                && x.id == y.id
                && x.attrs.len() == y.attrs.len()
                && x.attrs.iter().zip(&y.attrs).all(|((k, v), (l, w))| {
                    k == l
                        && match (v, w) {
                            // Bit for bit: `-0.0` is not `0.0` here.
                            (Value::Float(p), Value::Float(q)) => p.to_bits() == q.to_bits(),
                            _ => v == w,
                        }
                })
        })
}

#[test]
fn a_columnar_block_decodes_to_the_rows_written() {
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    for round in 0..300 {
        let n = [1, 2, 7, 64, 300][round % 5];
        let rows = docs(&mut r, n);
        let buf = encode_block(&rows);
        let want: Vec<(usize, Document)> = rows.iter().cloned().enumerate().collect();
        let all = decode_block(&buf, &Projection::ALL, None).unwrap();
        assert!(same(&all, &want), "round {round}");
        // Projected: the id dropped, two names kept.
        let keep = |name: &str| name == "a1" || name == "sorted";
        let projected = decode_block(
            &buf,
            &Projection {
                id: false,
                attrs: &keep,
            },
            None,
        )
        .unwrap();
        let want_projected: Vec<(usize, Document)> = want
            .iter()
            .map(|(i, d)| {
                let mut d = d.clone();
                d.id = String::new();
                d.attrs.retain(|k, _| keep(k));
                (*i, d)
            })
            .collect();
        assert!(same(&projected, &want_projected), "round {round}");
        // Picked: every third row.
        let pick = |i: usize| i % 3 == 1;
        let picked = decode_block(&buf, &Projection::ALL, Some(&pick)).unwrap();
        let want_picked: Vec<(usize, Document)> =
            want.iter().filter(|(i, _)| pick(*i)).cloned().collect();
        assert!(same(&picked, &want_picked), "round {round}");
    }
}

#[test]
fn a_truncated_or_corrupt_columnar_block_is_refused() {
    let mut r = Rng(0x1234_5678_9abc_def1);
    for _ in 0..20 {
        let rows = docs(&mut r, 40);
        let buf = encode_block(&rows);
        for cut in 0..buf.len() {
            assert!(
                decode_block(&buf[..cut], &Projection::ALL, None).is_err(),
                "cut at {cut} of {}",
                buf.len()
            );
        }
        let mut long = buf.clone();
        long.push(0);
        assert!(decode_block(&long, &Projection::ALL, None).is_err());
        // Any single byte changed decodes to an error or to rows -- never a panic.
        for at in 0..buf.len() {
            let mut bad = buf.clone();
            bad[at] ^= 0x5a;
            let _ = decode_block(&bad, &Projection::ALL, None);
        }
    }
    let mut bad = encode_block(&[Document::new("a", vec![])]);
    bad[0] = 2;
    assert!(decode_block(&bad, &Projection::ALL, None).is_err());
}

async fn open(bytes: bytes::Bytes) -> (MemoryStore, Key, Segment) {
    let s = MemoryStore::new();
    let key = Key::new("seg/1");
    s.put(&key, bytes).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    (s, key, seg)
}

/// The footer's version: `MAGIC(8) VERSION(2) ...` at a fixed distance from the end.
fn version(seg: &[u8]) -> u16 {
    let at = seg.len() - (8 + 2 + 8 + 4 + 4 + 8 + 8) + 8;
    u16::from_le_bytes([seg[at], seg[at + 1]])
}

#[tokio::test]
async fn columnar_blocks_answer_as_row_blocks_do() {
    let mut r = Rng(0xfeed_beef_dead_c0de);
    for n in [1, 9, 200, 3_000] {
        let rows = docs(&mut r, n);
        let build = |columnar: bool| {
            let mut w = SegmentWriter::new(16);
            if !columnar {
                w = w.with_row_blocks();
            }
            for d in &rows {
                w.push(d.clone());
            }
            w.try_finish().unwrap()
        };
        let (cb, rb) = (build(true), build(false));
        assert_eq!(version(&cb), 3, "{n} rows");
        assert!(version(&rb) < 3, "{n} rows");
        let (cs, ck, col) = open(cb).await;
        let (rs, rk, row) = open(rb).await;
        assert_eq!(col.block_count(), row.block_count(), "{n} rows");
        let all = |_: &Zones| true;
        let a = col.rows_where(&cs, &ck, all).await.unwrap();
        let b = row.rows_where(&rs, &rk, all).await.unwrap();
        assert!(same(&a, &b), "{n} rows: rows_where");
        let wanted: Vec<usize> = (0..n).step_by(7).chain([n - 1, 0]).collect();
        let a = col.rows_at(&cs, &ck, &wanted).await.unwrap();
        let b = row.rows_at(&rs, &rk, &wanted).await.unwrap();
        let pair = |v: Vec<Option<Document>>| -> Vec<(usize, Document)> {
            v.into_iter().map(|d| (0, d.unwrap())).collect()
        };
        assert!(same(&pair(a), &pair(b)), "{n} rows: rows_at");
        let a = col.ids_at(&cs, &ck, &wanted).await.unwrap();
        assert_eq!(a, row.ids_at(&rs, &rk, &wanted).await.unwrap());
        let keep = |name: &str| name == "a2";
        let project = Projection {
            id: true,
            attrs: &keep,
        };
        let mut a = Vec::new();
        col.visit_rows_projected(&cs, &ck, all, &project, |i, d| a.push((i, d)))
            .await
            .unwrap();
        let mut b = Vec::new();
        row.visit_rows_projected(&rs, &rk, all, &project, |i, d| b.push((i, d)))
            .await
            .unwrap();
        assert!(same(&a, &b), "{n} rows: projected");
        // The zones the two wrote agree, so pruning keeps the same blocks.
        for (x, y) in col.block_zones().zip(row.block_zones()) {
            assert_eq!(x.ints, y.ints);
            assert_eq!(x.floats, y.floats);
            assert_eq!(x.uints, y.uints);
        }
    }
}
