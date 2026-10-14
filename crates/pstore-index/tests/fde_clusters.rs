//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M71: the multi-vector encoding, clustered by a permutation sidecar.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::fde::{DEFAULT, encode_doc};
use pstore_index::cluster::Params;
use pstore_index::fde_clusters::{self, FdeClusters};
use pstore_types::TenantId;

const D: usize = 32;
const DOCS: usize = 1_500;

fn rnd(s: &mut u64) -> f32 {
    *s = s
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

fn topics(seed: u64) -> Vec<Vec<f32>> {
    let mut s = seed;
    (0..12)
        .map(|_| (0..D).map(|_| 2.0 * rnd(&mut s)).collect())
        .collect()
}

fn corpus(seed: u64) -> Vec<Vec<Vec<f32>>> {
    let t = topics(seed ^ 0x77);
    let mut s = seed;
    (0..DOCS)
        .map(|_| {
            let n = 8 + (rnd(&mut s).abs() * 16.0) as usize;
            let picks: Vec<usize> = (0..3)
                .map(|_| ((rnd(&mut s) + 1.0) * 6.0) as usize % 12)
                .collect();
            (0..n)
                .map(|i| t[picks[i % 3]].iter().map(|x| x + rnd(&mut s)).collect())
                .collect()
        })
        .collect()
}

fn queries(seed: u64) -> Vec<Vec<Vec<f32>>> {
    let t = topics(seed ^ 0x77);
    let mut s = seed ^ 0x1234;
    (0..20)
        .map(|_| {
            let n = 4 + (rnd(&mut s).abs() * 4.0) as usize;
            let a = ((rnd(&mut s) + 1.0) * 6.0) as usize % 12;
            let b = ((rnd(&mut s) + 1.0) * 6.0) as usize % 12;
            (0..n)
                .map(|i| {
                    t[if i % 2 == 0 { a } else { b }]
                        .iter()
                        .map(|x| x + 0.8 * rnd(&mut s))
                        .collect()
                })
                .collect()
        })
        .collect()
}

fn small() -> Params {
    Params {
        target_list_size: 125,
        exact_scan_threshold: 0,
        ..Params::default()
    }
}

fn encoded(docs: &[Vec<Vec<f32>>]) -> Vec<Vec<f32>> {
    docs.iter().map(|d| encode_doc(&DEFAULT, d)).collect()
}

fn key() -> Key {
    Key::new("t/idx/a.seg")
}

#[test]
fn sidecar_round_trips_and_refuses_damage() {
    let rows = encoded(&corpus(5)[..300]);
    let (c, _) = FdeClusters::build("late", &rows, small());
    assert!(c.lists() > 1);
    assert_eq!(c.field(), "late");
    let b = c.encode();
    assert_eq!(FdeClusters::decode(&b).unwrap(), c);
    assert!(FdeClusters::decode(&[]).is_none());
    assert!(
        FdeClusters::decode(&b[..b.len() - 1]).is_none(),
        "truncated"
    );
    assert!(
        FdeClusters::decode(&[b.clone(), vec![0]].concat()).is_none(),
        "trailing byte"
    );
    let mut magic = b.clone();
    magic[0] ^= 0xFF;
    assert!(FdeClusters::decode(&magic).is_none());
    // The row count is smaller than a list's row.
    let mut rows_field = b.clone();
    rows_field[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert!(
        FdeClusters::decode(&rows_field).is_none(),
        "a row past the end"
    );
    // More lists than the bytes can hold.
    let mut many = b.clone();
    many[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(FdeClusters::decode(&many).is_none());
    // A dimension too large for the bytes.
    let mut wide = b.clone();
    wide[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(FdeClusters::decode(&wide).is_none());
}

#[test]
fn build_is_deterministic_and_covers_every_row() {
    let rows = encoded(&corpus(5)[..400]);
    let (a, blob) = FdeClusters::build("late", &rows, small());
    let (again, blob2) = FdeClusters::build("late", &rows, small());
    assert_eq!((a.encode(), blob.clone()), (again.encode(), blob2));
    let decoded = FdeClusters::decode(&a.encode()).unwrap();
    assert_eq!(decoded, a);
    // Every row is in a list: search every list and see every row.
    let mut seen = vec![false; rows.len()];
    for r in 0..rows.len() {
        let _ = r;
    }
    let raw = a.encode();
    let mut at = 24 + a.lists() * rows[0].len() * 4;
    for _ in 0..a.lists() {
        let n = u32::from_le_bytes(raw[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        for _ in 0..n {
            let r = u32::from_le_bytes(raw[at..at + 4].try_into().unwrap()) as usize;
            seen[r] = true;
            at += 4;
        }
    }
    assert!(seen.iter().all(|s| *s), "every row sits in a list");
    assert_eq!(at, raw.len());
    assert_eq!(fde_clusters::key(&key()).as_str(), "t/idx/a.seg.fdec");
    assert_eq!(
        fde_clusters::vectors_key(&key()).as_str(),
        "t/idx/a.seg.fdev"
    );
    // The vectors blob holds one row per list entry, in list order.
    let entries = a.encode().len();
    assert!(entries > 0);
    assert_eq!(blob.len() % (rows[0].len() * 4), 0);
    let first = u32::from_le_bytes(
        raw[24 + a.lists() * rows[0].len() * 4 + 4..][..4]
            .try_into()
            .unwrap(),
    ) as usize;
    let want: Vec<u8> = rows[first].iter().flat_map(|x| x.to_le_bytes()).collect();
    assert_eq!(
        &blob[..want.len()],
        want.as_slice(),
        "the first entry is the first list's first row"
    );
}

fn overlap(a: &[(usize, f32)], b: &[(usize, f32)]) -> f64 {
    a.iter().filter(|x| b.iter().any(|y| y.0 == x.0)).count() as f64 / a.len().max(1) as f64
}

#[tokio::test]
async fn clustered_search_finds_what_the_flat_scan_finds() {
    let docs = corpus(11);
    let store = MemoryStore::new();
    let (sidecar, blob) = FdeClusters::build("late", &encoded(&docs), small());
    sidecar.put(&store, &key(), blob).await.unwrap();
    let opened = FdeClusters::open(&store, &key()).await.unwrap();
    assert_eq!(opened, sidecar);

    let other_store = MemoryStore::new();
    let (other, other_blob) = FdeClusters::build("late", &encoded(&corpus(99)), small());
    other.put(&other_store, &key(), other_blob).await.unwrap();
    let (mut got, mut control, mut n) = (0.0, 0.0, 0.0);
    for q in queries(11) {
        // The flat scan is the same search with every list probed.
        let flat = opened
            .search(&store, &key(), &q, usize::MAX, 10)
            .await
            .unwrap();
        let part = opened.search(&store, &key(), &q, 4, 10).await.unwrap();
        let wrong = other.search(&other_store, &key(), &q, 4, 10).await.unwrap();
        got += overlap(&part, &flat);
        control += overlap(&wrong, &flat);
        n += 1.0;
    }
    let (got, control) = (got / n, control / n);
    eprintln!("overlap {got} control {control} lists {}", sidecar.lists());
    assert!(got >= FLOOR, "overlap {got}");
    assert!(control < FLOOR, "control {control}");
    // A vectors blob of the wrong size is an error, not a wrong answer.
    store
        .put(&fde_clusters::vectors_key(&key()), vec![0u8; 100].into())
        .await
        .unwrap();
    assert!(
        opened
            .search(&store, &key(), &queries(11)[0], 4, 10)
            .await
            .is_err()
    );
    // A missing sidecar is an error.
    assert!(
        FdeClusters::open(&store, &Key::new("t/idx/none.seg"))
            .await
            .is_err()
    );
}

/// Measured on the first run: 0.86, with the control at 0.355 (provisional; deterministic data).
const FLOOR: f64 = 0.8;

#[tokio::test]
async fn clustered_search_reads_a_fraction() {
    let docs = corpus(11);
    let q = &queries(11)[0];
    let bytes_at = |probes: usize| {
        let docs = docs.clone();
        async move {
            let acct = Accounted::new(MemoryStore::new());
            let store = acct.as_tenant(TenantId(1));
            let (sidecar, blob) = FdeClusters::build("late", &encoded(&docs), small());
            sidecar.put(&store, &key(), blob).await.unwrap();
            acct.record_ranges();
            sidecar.search(&store, &key(), q, probes, 10).await.unwrap();
            acct.bytes_in(&fde_clusters::vectors_key(&key()), 0..u64::MAX / 2)
        }
    };
    let (flat, part) = (bytes_at(usize::MAX).await, bytes_at(4).await);
    eprintln!("flat {flat} part {part}");
    assert!(part * 2 < flat, "{part} of {flat}");
}

use pstore_index::fde_clusters::FdeSet;

fn two_fields() -> Vec<(String, Vec<Vec<f32>>)> {
    vec![
        ("a".to_owned(), encoded(&corpus(11)[..600])),
        ("b".to_owned(), encoded(&corpus(99)[..600])),
    ]
}

#[test]
fn a_set_round_trips_and_refuses_damage() {
    let (set, blob) = FdeSet::build(&two_fields(), small());
    assert_eq!(set.len(), 2);
    assert!(!set.is_empty() && set.worth_writing());
    let b = set.encode();
    assert_eq!(FdeSet::decode(&b).unwrap(), set);
    assert!(set.field("a").unwrap().1 == 0);
    let second = set.field("b").unwrap().1;
    assert!(second > 0 && (second as usize) < blob.len());
    assert!(set.field("c").is_none());
    assert!(FdeSet::decode(&[]).is_none());
    assert!(FdeSet::decode(&b[..b.len() - 1]).is_none(), "truncated");
    assert!(
        FdeSet::decode(&[b.clone(), vec![0]].concat()).is_none(),
        "trailing"
    );
    let mut magic = b.clone();
    magic[0] ^= 0xFF;
    assert!(FdeSet::decode(&magic).is_none());
    let mut many = b.clone();
    many[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(FdeSet::decode(&many).is_none(), "a count above the bytes");
    // A field twice.
    let twice = vec![two_fields().remove(0), two_fields().remove(0)];
    let (dup, _) = FdeSet::build(&twice, small());
    assert!(FdeSet::decode(&dup.encode()).is_none());
    // A single-cluster set is not worth writing.
    let (tiny, _) = FdeSet::build(
        &[("a".to_owned(), encoded(&corpus(5)[..10]))],
        Params::default(),
    );
    assert!(!tiny.worth_writing());
    assert!(FdeSet::build(&[], small()).0.is_empty());
}

#[tokio::test]
async fn each_field_searches_its_own_vectors() {
    let fields = two_fields();
    let store = MemoryStore::new();
    let (set, blob) = FdeSet::build(&fields, small());
    set.put(&store, &key(), blob).await.unwrap();
    let opened = FdeSet::open(&store, &key()).await.unwrap();
    assert_eq!(opened, set);
    for (name, rows) in &fields {
        let alone_store = MemoryStore::new();
        let (alone, alone_blob) = FdeClusters::build(name, rows, small());
        alone.put(&alone_store, &key(), alone_blob).await.unwrap();
        let (c, base) = opened.field(name).unwrap();
        for q in queries(11).iter().take(5) {
            let want = alone.search(&alone_store, &key(), q, 3, 10).await.unwrap();
            let got = c.search_at(&store, &key(), base, q, 3, 10).await.unwrap();
            assert_eq!(got, want, "field {name}");
        }
    }
    assert!(FdeSet::open(&store, &Key::new("t/none.seg")).await.is_err());
}

/// M83: a clustered vector query, in the clustered space as given (no encoding).
#[tokio::test]
async fn a_vector_query_scores_the_probed_lists_exactly() {
    let mut s = 83u64;
    let rows: Vec<Vec<f32>> = (0..9_000)
        .map(|i| {
            let topic = (i % 12) as f32;
            (0..4)
                .map(|d| topic * (d as f32 + 1.0) + rnd(&mut s))
                .collect()
        })
        .collect();
    let (clusters, blob) = FdeClusters::build("x", &rows, Params::default());
    assert!(clusters.lists() > 1);
    let store = MemoryStore::new();
    let seg = Key::new("t/idx/s.seg");
    store
        .put(&fde_clusters::vectors_key(&seg), blob.into())
        .await
        .unwrap();
    let q = vec![3.0f32, 6.0, 9.0, 12.0];
    let dot = |v: &[f32]| v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>();

    // Every list: exactly the brute-force top 10, scores to the bit.
    let all = clusters
        .search_vector_at(&store, &seg, 0, &q, 1 << 20, 10)
        .await
        .unwrap();
    let mut want: Vec<(usize, f32)> = rows.iter().enumerate().map(|(i, v)| (i, dot(v))).collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    want.truncate(10);
    assert_eq!(all, want);

    // One list: a subset of what exists, each score the true dot product, best first.
    let one = clusters
        .search_vector_at(&store, &seg, 0, &q, 1, 10)
        .await
        .unwrap();
    assert!(!one.is_empty() && one.len() <= 10);
    for (row, score) in &one {
        assert_eq!(*score, dot(&rows[*row]));
    }
    assert!(one.windows(2).all(|w| w[0].1 >= w[1].1));
    assert_eq!(one[0], want[0], "the best row is in the best list");

    // A vector blob not the size the lists imply is corrupt, not short.
    let short = MemoryStore::new();
    short
        .put(&fde_clusters::vectors_key(&seg), vec![0u8; 8].into())
        .await
        .unwrap();
    assert!(
        clusters
            .search_vector_at(&short, &seg, 0, &q, 1, 10)
            .await
            .is_err()
    );
    // And a field's `base` shifts where its vectors are read.
    let mut padded = vec![0u8; 64];
    padded.extend(store.get(&fde_clusters::vectors_key(&seg)).await.unwrap());
    let shifted = MemoryStore::new();
    shifted
        .put(&fde_clusters::vectors_key(&seg), padded.into())
        .await
        .unwrap();
    let at = clusters
        .search_vector_at(&shifted, &seg, 64, &q, 1 << 20, 10)
        .await
        .unwrap();
    assert_eq!(at, want);
}

#[test]
fn a_dense_field_never_shares_a_name_with_a_multi_vector_one() {
    assert_eq!(fde_clusters::dense_name("tag"), "\0dense\0tag");
    assert_ne!(fde_clusters::dense_name("tag"), "tag");
    assert!(fde_clusters::dense_name("tag").starts_with('\0'));
}
