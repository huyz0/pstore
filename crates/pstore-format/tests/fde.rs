//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M67: the fixed-dimensional encoding is the algorithm, and finds what exact MaxSim finds.

use pstore_format::fde::{DEFAULT, Params, encode_doc, encode_query, field_name};

/// A deterministic pseudo-random float in -1..1.
fn rnd(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

fn vecs(seed: u64, n: usize, d: usize) -> Vec<Vec<f32>> {
    let mut s = seed;
    (0..n)
        .map(|_| (0..d).map(|_| rnd(&mut s)).collect())
        .collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn maxsim(q: &[Vec<f32>], d: &[Vec<f32>]) -> f32 {
    q.iter()
        .map(|q| {
            d.iter()
                .map(|d| dot(q, d))
                .fold(f32::NEG_INFINITY, f32::max)
        })
        .sum()
}

#[test]
fn fde_is_deterministic_and_fixed_width() {
    assert_eq!(field_name("late"), "late$fde");
    for (d, n) in [(8, 1), (16, 5), (128, 40), (128, 1000)] {
        let set = vecs(7, n, d);
        let a = encode_doc(&DEFAULT, &set);
        assert_eq!(a.len(), 512, "d {d} n {n}");
        assert_eq!(DEFAULT.dim(d), 512);
        assert_eq!(a, encode_doc(&DEFAULT, &set));
        let q = encode_query(&DEFAULT, &set);
        assert_eq!(q.len(), 512);
        assert!(a.iter().all(|x| x.is_finite()));
    }
    // A different seed is a different encoding.
    let set = vecs(7, 5, 16);
    let other = Params {
        seed: DEFAULT.seed + 1,
        ..DEFAULT
    };
    assert_ne!(encode_doc(&DEFAULT, &set), encode_doc(&other, &set));
    // Nothing in, nothing out.
    assert!(encode_doc(&DEFAULT, &[]).is_empty());
    // A query of one vector is that vector, summed alone: it scales linearly.
    let one = vecs(3, 1, 16);
    let twice = vec![one[0].clone(), one[0].clone()];
    let e1 = encode_query(&DEFAULT, &one);
    let e2 = encode_query(&DEFAULT, &twice);
    for (a, b) in e1.iter().zip(&e2) {
        assert!((2.0 * a - b).abs() < 1e-5);
    }
    // ...while a document of the same vector twice is the same encoding as once (a mean).
    let d1 = encode_doc(&DEFAULT, &one);
    let d2 = encode_doc(&DEFAULT, &twice);
    for (a, b) in d1.iter().zip(&d2) {
        assert!((a - b).abs() < 1e-5);
    }
}

#[test]
fn fde_with_one_bucket_is_sum_and_mean() {
    let p = Params {
        reps: 1,
        k_sim: 0,
        d_proj: 0,
        seed: 1,
    };
    let doc = vec![vec![1.0, 0.0, 2.0], vec![3.0, 4.0, 0.0]];
    let query = vec![vec![1.0, 1.0, 1.0], vec![0.0, 2.0, -1.0]];
    let d = encode_doc(&p, &doc);
    let q = encode_query(&p, &query);
    assert_eq!(d, vec![2.0, 2.0, 1.0], "the mean");
    assert_eq!(q, vec![1.0, 3.0, 0.0], "the sum");
    // The dot product is sum_q q . mean(d), by hand: 1*2+3*2+0*1.
    assert_eq!(dot(&q, &d), 8.0);
    assert_eq!(p.dim(3), 3);
}

#[test]
fn fde_fills_an_empty_bucket_from_the_nearest() {
    // One bit, two buckets, no projection. A document whose vectors all fall in one bucket
    // has the other bucket equal to that bucket's mean... the nearest by Hamming is the only
    // occupied one, so the empty bucket takes its (single) vector.
    let p = Params {
        reps: 1,
        k_sim: 1,
        d_proj: 0,
        seed: 5,
    };
    let v = vec![vec![1.0, 2.0, 3.0, 4.0]];
    let e = encode_doc(&p, &v);
    assert_eq!(e.len(), 8);
    let (a, b) = e.split_at(4);
    assert_eq!(a, b, "both buckets hold the one vector");
    assert_eq!(a, v[0].as_slice());
    // The query leaves it empty (zero).
    let q = encode_query(&p, &v);
    let zeros = q.chunks(4).filter(|c| c.iter().all(|x| *x == 0.0)).count();
    assert_eq!(zeros, 1);
    // Two vectors in two buckets: each keeps its own, nothing is filled.
    let mut found = false;
    for s in 0..50 {
        let two = vecs(100 + s, 2, 4);
        let q = encode_query(&p, &two);
        if q.chunks(4).all(|c| c.iter().any(|x| *x != 0.0)) {
            let d = encode_doc(&p, &two);
            assert_ne!(&d[..4], &d[4..]);
            found = true;
            break;
        }
    }
    assert!(found, "a seed that splits two vectors");
}

/// Clustered corpus: each document draws its vectors around a few of the topic centers.
fn corpus(seed: u64) -> Vec<Vec<Vec<f32>>> {
    let d = 32;
    let topics = vecs(seed ^ 0x77, 12, d);
    let mut s = seed;
    (0..300)
        .map(|_| {
            let n = 8 + (rnd(&mut s).abs() * 16.0) as usize;
            let picks: Vec<usize> = (0..3)
                .map(|_| ((rnd(&mut s) + 1.0) * 6.0) as usize % 12)
                .collect();
            (0..n)
                .map(|i| {
                    let t = &topics[picks[i % 3]];
                    t.iter().map(|x| x + 1.0 * rnd(&mut s)).collect()
                })
                .collect()
        })
        .collect()
}

fn queries(seed: u64) -> Vec<Vec<Vec<f32>>> {
    let d = 32;
    let topics = vecs(seed ^ 0x77, 12, d);
    let mut s = seed ^ 0x1234;
    (0..20)
        .map(|_| {
            let n = 4 + (rnd(&mut s).abs() * 4.0) as usize;
            let a = ((rnd(&mut s) + 1.0) * 6.0) as usize % 12;
            let b = ((rnd(&mut s) + 1.0) * 6.0) as usize % 12;
            (0..n)
                .map(|i| {
                    let t = &topics[if i % 2 == 0 { a } else { b }];
                    t.iter().map(|x| x + 0.8 * rnd(&mut s)).collect()
                })
                .collect()
        })
        .collect()
}

/// Mean recall of the exact top 10 within the top 100 of a ranking by `score`.
fn recall(
    docs: &[Vec<Vec<f32>>],
    qs: &[Vec<Vec<f32>>],
    score: impl Fn(usize, usize) -> f32,
) -> f64 {
    let mut total = 0.0;
    for (qi, q) in qs.iter().enumerate() {
        let mut exact: Vec<(usize, f32)> = docs
            .iter()
            .enumerate()
            .map(|(i, d)| (i, maxsim(q, d)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let top: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
        let mut approx: Vec<(usize, f32)> = (0..docs.len()).map(|i| (i, score(qi, i))).collect();
        approx.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let window: Vec<usize> = approx.iter().take(100).map(|e| e.0).collect();
        total += top.iter().filter(|t| window.contains(t)).count() as f64 / 10.0;
    }
    total / qs.len() as f64
}

#[test]
fn fde_candidates_recover_the_exact_top() {
    let docs = corpus(11);
    let qs = queries(11);
    let denc: Vec<Vec<f32>> = docs.iter().map(|d| encode_doc(&DEFAULT, d)).collect();
    let qenc: Vec<Vec<f32>> = qs.iter().map(|q| encode_query(&DEFAULT, q)).collect();
    let got = recall(&docs, &qs, |q, d| dot(&qenc[q], &denc[d]));
    // Control: the same ranker over documents encoded from a corpus of another seed, so it
    // ranks by nothing about these documents.
    let other = corpus(99);
    let wrong: Vec<Vec<f32>> = other.iter().map(|d| encode_doc(&DEFAULT, d)).collect();
    let control = recall(&docs, &qs, |q, d| dot(&qenc[q], &wrong[d]));
    eprintln!("recall {got} control {control}");
    assert!(got >= FLOOR, "recall@10 in 100 {got} below {FLOOR}");
    assert!(control < FLOOR, "the control ranker reached {control}");
}

/// Measured on the first run on a 4-core container (0.98; provisional, the data is deterministic). The control ranked 0.40.
const FLOOR: f64 = 0.9;

// ---- M67: Segment::field_rows ---------------------------------------------------------

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, Segment, SegmentWriter, VectorField};

#[tokio::test]
async fn field_rows_reads_only_the_named_rows() {
    let acct = Accounted::new(MemoryStore::new());
    let store = acct.as_tenant(pstore_types::TenantId(1));
    let mut w = SegmentWriter::new(4);
    let mut want: Vec<Vec<Vec<f32>>> = Vec::new();
    for i in 0..3000usize {
        let mut d = Document::new(format!("d{i:02}"), vec![i as f32, 1.0, 0.0, 0.5]);
        // Varying counts, one row with a single vector, a width of 6, and some with none.
        let n = if i % 11 == 5 { 0 } else { 1 + (i * 7) % 5 };
        let vs: Vec<Vec<f32>> = (0..n)
            .map(|j| (0..6).map(|k| (i * 10 + j * 3 + k) as f32).collect())
            .collect();
        want.push(vs.clone());
        if !vs.is_empty() {
            d.vectors.insert("late".to_owned(), VectorField::Dense(vs));
        }
        // A fixed-width single-vector field beside it.
        d.vectors.insert(
            "one".to_owned(),
            VectorField::Dense(vec![vec![i as f32, -(i as f32), 2.0]]),
        );
        w.push(d);
    }
    let key = Key::new("t/idx/a.seg");
    store.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(&store, &key).await.unwrap();

    let all = seg.field_vectors(&store, &key, "late").await.unwrap();
    let rows = [3usize, 4, 5, 16, 1500, 2999, 3, 3000, 100_000];
    acct.record_ranges();
    let got = seg.field_rows(&store, &key, "late", &rows).await.unwrap();
    assert_eq!(
        got.len(),
        6,
        "a duplicate and rows at and past the end are dropped"
    );
    assert!(
        got[&5].is_empty() && got[&16].is_empty(),
        "rows holding none"
    );
    for (r, vs) in &got {
        assert_eq!(vs, &all[*r], "row {r}");
        assert_eq!(vs, &want[*r]);
    }
    // Only the table entries and the rows' vectors moved, not the field.
    let span = seg.field_section("late", Section::Vectors).unwrap();
    let moved = acct.bytes_in(&key, span.clone());
    let whole = span.end - span.start;
    assert!(moved * 4 < whole, "{moved} of {whole} bytes");

    // A single-vector field is a computable range a row.
    let one = seg.field_vectors(&store, &key, "one").await.unwrap();
    let got = seg
        .field_rows(&store, &key, "one", &[0, 2999])
        .await
        .unwrap();
    assert_eq!(got[&0], one[0]);
    assert_eq!(got[&2999], one[2999]);
    // An unknown field is an error; nothing asked is nothing read.
    assert!(seg.field_rows(&store, &key, "nope", &[0]).await.is_err());
    assert!(
        seg.field_rows(&store, &key, "late", &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn field_rows_refuses_a_corrupt_table_and_skips_a_sparse_field() {
    let store = MemoryStore::new();
    let mut w = SegmentWriter::new(4);
    let mut all = Vec::new();
    for i in 0..40usize {
        let mut d = Document::new(format!("d{i:02}"), vec![i as f32, 1.0, 0.0, 0.5]);
        let vs: Vec<Vec<f32>> = (0..2 + i % 3).map(|j| vec![(i + j) as f32; 5]).collect();
        d.vectors.insert("late".to_owned(), VectorField::Dense(vs));
        d.vectors.insert(
            "terms".to_owned(),
            VectorField::Sparse(vec![
                (3, pstore_format::Impact::new(1.0)),
                (9, pstore_format::Impact::new(2.0)),
            ]),
        );
        all.push(d.clone());
        w.push(d);
    }
    let postings =
        pstore_format::sparse::build(&all, "terms", pstore_format::sparse::DEFAULT_ENCODING);
    let key = Key::new("t/idx/c.seg");
    let bytes = w
        .with_section(Section::SparsePostings, postings.section)
        .try_finish()
        .unwrap();
    store.put(&key, bytes.clone()).await.unwrap();
    let seg = Segment::open(&store, &key).await.unwrap();
    // A sparse field has no dense rows to return.
    assert!(
        seg.field_rows(&store, &key, "terms", &[0, 1])
            .await
            .unwrap()
            .is_empty()
    );

    let span = seg.field_section("late", Section::Vectors).unwrap();
    let table = 41 * 8;
    let end = span.end;
    let corrupt = |row: usize, value: u64| {
        let mut b = bytes.to_vec();
        let at = span.start as usize + row * 8;
        b[at..at + 8].copy_from_slice(&value.to_le_bytes());
        b
    };
    // An offset past the section, and an offset above the next.
    for (what, b) in [
        ("past the end", corrupt(7, end)),
        ("above the next", corrupt(7, u64::MAX / 4)),
        ("not ascending", corrupt(8, 0)),
    ] {
        let s2 = MemoryStore::new();
        s2.put(&key, b.into()).await.unwrap();
        let seg = Segment::open(&s2, &key).await.unwrap();
        let _ = table;
        assert!(
            seg.field_rows(&s2, &key, "late", &[7]).await.is_err(),
            "{what}"
        );
    }
}

/// The bucket a single vector falls in, read back from the query encoding of one repetition
/// with no projection: exactly one chunk is the vector, the rest zero.
fn bucket_of(p: &Params, v: &[f32]) -> usize {
    let e = encode_query(p, &[v.to_vec()]);
    let d = v.len();
    let hits: Vec<usize> = e
        .chunks(d)
        .enumerate()
        .filter(|(_, c)| c.iter().any(|x| *x != 0.0))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hits.len(), 1, "a vector lands in one bucket");
    hits[0]
}

#[test]
fn fde_buckets_are_balanced_and_nearby_vectors_share_one() {
    let p = Params {
        reps: 1,
        k_sim: 3,
        d_proj: 0,
        seed: 42,
    };
    let pts = vecs(5, 800, 24);
    let mut counts = [0usize; 8];
    for v in &pts {
        counts[bucket_of(&p, v)] += 1;
    }
    // Eight buckets from three hyperplanes through the origin: every one used, none hoarding.
    assert!(
        counts.iter().all(|c| (40..=220).contains(c)),
        "unbalanced: {counts:?}"
    );
    // A small nudge keeps a vector's bucket far more often than an unrelated vector shares it.
    let mut near = 0;
    let mut far = 0;
    let mut s = 99u64;
    for (i, v) in pts.iter().enumerate() {
        let nudged: Vec<f32> = v.iter().map(|x| x + 0.03 * rnd(&mut s)).collect();
        near += usize::from(bucket_of(&p, &nudged) == bucket_of(&p, v));
        far += usize::from(bucket_of(&p, &pts[(i + 1) % pts.len()]) == bucket_of(&p, v));
    }
    assert!(near * 10 > pts.len() * 9, "near {near} of {}", pts.len());
    assert!(
        far * 4 < pts.len(),
        "unrelated pairs collide {far} of {}",
        pts.len()
    );
}

#[test]
fn fde_projection_keeps_norms_roughly() {
    let p0 = Params {
        reps: 2,
        k_sim: 2,
        d_proj: 0,
        seed: 7,
    };
    let p1 = Params { d_proj: 16, ..p0 };
    let (mut full, mut proj) = (0.0f64, 0.0f64);
    for seed in 0..60 {
        let set = vecs(seed, 6, 48);
        let n = |e: &[f32]| e.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>();
        full += n(&encode_doc(&p0, &set));
        proj += n(&encode_doc(&p1, &set));
    }
    let ratio = proj / full;
    assert!((0.7..1.4).contains(&ratio), "squared norm ratio {ratio}");
}

#[test]
fn fde_fills_an_empty_bucket_by_hamming_distance_not_by_order() {
    let p = Params {
        reps: 1,
        k_sim: 2,
        d_proj: 0,
        seed: 3,
    };
    // Find two vectors in buckets 0b00 (earlier) and 0b01 (later): the empty bucket 0b11 is
    // one bit from 0b01 and two from 0b00, so it takes the LATER vector.
    let pts = vecs(17, 400, 6);
    let a = pts
        .iter()
        .find(|v| bucket_of(&p, v) == 0b00)
        .unwrap()
        .clone();
    let b = pts
        .iter()
        .find(|v| bucket_of(&p, v) == 0b01)
        .unwrap()
        .clone();
    let e = encode_doc(&p, &[a.clone(), b.clone()]);
    let chunk = |i: usize| e[i * 6..(i + 1) * 6].to_vec();
    assert_eq!(chunk(0b00), a);
    assert_eq!(chunk(0b01), b);
    assert_eq!(chunk(0b11), b, "nearest by Hamming, not the earliest");
    // 0b10 is one bit from 0b00 and two from 0b01: the earlier one.
    assert_eq!(chunk(0b10), a);
}
