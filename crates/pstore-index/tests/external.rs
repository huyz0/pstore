//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M69: an index over vectors left in a customer's object.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::external::ExternalColumn;
use pstore_index::external::ExternalIndex;
use pstore_types::TenantId;

const DIMS: usize = 64;
const ROWS: usize = 20_000;
const HEADER: usize = 32;
const GAP: usize = 8;
const STRIDE: usize = DIMS * 4 + GAP;

fn rnd(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

/// 40 topics, each row near one.
fn vectors(seed: u64, rows: usize) -> Vec<Vec<f32>> {
    let mut s = seed;
    let topics: Vec<Vec<f32>> = (0..40)
        .map(|_| (0..DIMS).map(|_| rnd(&mut s)).collect())
        .collect();
    (0..rows)
        .map(|i| {
            let t = &topics[i % 40];
            t.iter().map(|x| x + 0.6 * rnd(&mut s)).collect()
        })
        .collect()
}

fn file(vs: &[Vec<f32>]) -> Vec<u8> {
    let mut b = vec![0x11; HEADER];
    for v in vs {
        for x in v {
            b.extend(x.to_le_bytes());
        }
        b.extend([0x22; GAP]);
    }
    b.extend([0x33; 100]);
    b
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut d, mut x, mut y) = (0.0f64, 0.0f64, 0.0f64);
    for (p, q) in a.iter().zip(b) {
        d += f64::from(*p) * f64::from(*q);
        x += f64::from(*p).powi(2);
        y += f64::from(*q).powi(2);
    }
    d / (x.sqrt() * y.sqrt())
}

fn data_key() -> Key {
    Key::new("lake/embeddings.bin")
}
fn codes_key() -> Key {
    Key::new("t/ext/codes")
}

async fn column<S: BlobStore>(store: &S, rows: usize) -> ExternalColumn {
    ExternalColumn::pin(
        store,
        &data_key(),
        HEADER as u64,
        DIMS as u32,
        STRIDE as u64,
        rows as u64,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn build_writes_codes_and_no_floats() {
    let vs = vectors(1, 2_000);
    let store = MemoryStore::new();
    store.put(&data_key(), file(&vs).into()).await.unwrap();
    let col = column(&store, 2_000).await;
    ExternalIndex::build(&store, &codes_key(), col.clone(), 512)
        .await
        .unwrap();
    let blob = store.get(&codes_key()).await.unwrap();
    assert!(
        blob.len() * 8 < col.len as usize,
        "{} of {}",
        blob.len(),
        col.len
    );
    // No run of eight consecutive floats of the customer's data appears in the blob.
    for v in vs.iter().take(50) {
        let run: Vec<u8> = v[..8].iter().flat_map(|x| x.to_le_bytes()).collect();
        assert!(!blob.windows(run.len()).any(|w| w == run.as_slice()));
    }
    // A zero vector has no direction.
    let mut zeros = vs.clone();
    zeros[7] = vec![0.0; DIMS];
    let s2 = MemoryStore::new();
    s2.put(&data_key(), file(&zeros).into()).await.unwrap();
    let c2 = column(&s2, 2_000).await;
    assert!(
        ExternalIndex::build(&s2, &codes_key(), c2, 512)
            .await
            .is_err()
    );
}

async fn recall(
    index: &ExternalIndex,
    store: &impl BlobStore,
    vs: &[Vec<f32>],
    window: usize,
) -> f64 {
    let mut s = 99;
    let mut hit = 0usize;
    for _ in 0..20 {
        let q: Vec<f32> = vs[(rnd(&mut s).abs() * 19_000.0) as usize]
            .iter()
            .map(|x| x + 0.3 * rnd(&mut s))
            .collect();
        let mut exact: Vec<(usize, f64)> = vs
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(&q, v)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let got = index.search(store, &q, 10, window).await.unwrap();
        assert_eq!(got.len(), 10);
        for w in got.windows(2) {
            assert!(w[0].1 >= w[1].1, "descending");
        }
        for (row, score) in &got {
            assert!((f64::from(*score) - cosine(&q, &vs[*row])).abs() < 1e-5);
        }
        hit += exact[..10]
            .iter()
            .filter(|e| got.iter().any(|g| g.0 == e.0))
            .count();
    }
    hit as f64 / 200.0
}

#[tokio::test]
async fn search_finds_the_exact_neighbours() {
    let vs = vectors(5, ROWS);
    let store = MemoryStore::new();
    store.put(&data_key(), file(&vs).into()).await.unwrap();
    let col = column(&store, ROWS).await;
    let index = ExternalIndex::build(&store, &codes_key(), col, 4096)
        .await
        .unwrap();
    let got = recall(&index, &store, &vs, 200).await;
    eprintln!("recall {got}");
    assert!(got >= FLOOR, "recall {got}");
    // Control: the same customer data, searched through another column's codes.
    let other = vectors(77, ROWS);
    let s2 = MemoryStore::new();
    s2.put(&data_key(), file(&other).into()).await.unwrap();
    let c2 = column(&s2, ROWS).await;
    let wrong = ExternalIndex::build(&s2, &codes_key(), c2, 4096)
        .await
        .unwrap();
    // Wrong codes, right rows: the customer's object is `vs`, the codes are `other`'s.
    let mixed = wrong.with_column(column(&store, ROWS).await);
    let control = recall(&mixed, &store, &vs, 200).await;
    eprintln!("control {control}");
    assert!(control < FLOOR, "control {control}");
}

/// Measured on the first run: 0.95, with the control at 0.01 (provisional; deterministic data).
const FLOOR: f64 = 0.9;

#[tokio::test]
async fn search_reads_only_the_candidates() {
    let vs = vectors(5, ROWS);
    let acct = Accounted::new(MemoryStore::new());
    let store = acct.as_tenant(TenantId(1));
    store.put(&data_key(), file(&vs).into()).await.unwrap();
    let col = column(&store, ROWS).await;
    let index = ExternalIndex::build(&store, &codes_key(), col.clone(), 4096)
        .await
        .unwrap();
    let reopened = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    acct.record_ranges();
    reopened.search(&store, &vs[3], 10, 100).await.unwrap();
    let moved = acct.bytes_in(&data_key(), 0..col.len);
    // 100 scattered candidates: the coalescer fetches a hole cheaper to read than to skip, so
    // this is a fraction of the object, not 100 rows' worth -- and the codes' own read is not
    // the customer's object at all.
    assert!(moved * 2 < col.len, "{moved} of {}", col.len);
    assert!(
        moved >= 100 * (DIMS as u64) * 4 / 2,
        "candidates were read: {moved}"
    );
    drop(index);
}

#[tokio::test]
async fn open_round_trips_and_refuses_damage() {
    let vs = vectors(3, 3_000);
    let store = MemoryStore::new();
    store.put(&data_key(), file(&vs).into()).await.unwrap();
    let col = column(&store, 3_000).await;
    let built = ExternalIndex::build(&store, &codes_key(), col, 1000)
        .await
        .unwrap();
    let opened = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    for q in [&vs[1], &vs[2_999]] {
        assert_eq!(
            built.search(&store, q, 5, 50).await.unwrap(),
            opened.search(&store, q, 5, 50).await.unwrap()
        );
    }
    let blob = store.get(&codes_key()).await.unwrap().to_vec();
    let put = |b: Vec<u8>, name: &'static str| {
        let store = store.clone();
        async move {
            let k = Key::new(format!("t/ext/{name}"));
            store.put(&k, b.into()).await.unwrap();
            ExternalIndex::open(&store, &k).await
        }
    };
    assert!(put(blob[..blob.len() - 1].to_vec(), "short").await.is_err());
    assert!(put([blob.clone(), vec![0]].concat(), "long").await.is_err());
    assert!(put(blob[..8].to_vec(), "head").await.is_err());
    let mut magic = blob.clone();
    magic[0] ^= 0xFF;
    assert!(put(magic, "magic").await.is_err());
    let mut dims = blob.clone();
    dims[4] ^= 0x01;
    assert!(put(dims, "dims").await.is_err());
    assert!(
        ExternalIndex::open(&store, &Key::new("t/ext/none"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_changed_source_is_refused() {
    let vs = vectors(3, 3_000);
    let store = MemoryStore::new();
    store.put(&data_key(), file(&vs).into()).await.unwrap();
    let col = column(&store, 3_000).await;
    let index = ExternalIndex::build(&store, &codes_key(), col, 1000)
        .await
        .unwrap();
    assert!(index.search(&store, &vs[0], 5, 50).await.is_ok());
    let mut changed = vs.clone();
    changed[0][0] += 1.0;
    store.put(&data_key(), file(&changed).into()).await.unwrap();
    assert!(index.search(&store, &vs[0], 5, 50).await.is_err());
    // A query of the wrong width is refused too.
    let fresh = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    let _ = fresh;
}

#[tokio::test]
async fn a_tagless_pin_round_trips_and_errors_say_what_they_are() {
    let vs = vectors(3, 500);
    let store = MemoryStore::new();
    store.put(&data_key(), file(&vs).into()).await.unwrap();
    let col = column(&store, 500).await;
    assert!(col.tag.is_some(), "the memory store tags its objects");
    let tagged = ExternalIndex::build(&store, &codes_key(), col.clone(), 100)
        .await
        .unwrap();
    // The same codes pinned without a tag (a store that has none): one flag byte, no tag.
    let mut bare = col;
    bare.tag = None;
    let tagless = tagged.clone().with_column(bare);
    let (a, b) = (tagged.to_bytes(), tagless.to_bytes());
    assert!(
        b.len() < a.len(),
        "no tag bytes: {} vs {}",
        b.len(),
        a.len()
    );
    assert_eq!(ExternalIndex::from_bytes(&b).unwrap().to_bytes(), b);
    assert_eq!(ExternalIndex::from_bytes(&a).unwrap().to_bytes(), a);

    // Every error says what it is.
    let corrupt = ExternalIndex::from_bytes(&[]).unwrap_err().to_string();
    let missing = ExternalIndex::open(&store, &Key::new("t/none"))
        .await
        .unwrap_err()
        .to_string();
    let vector = tagged
        .search(&store, &[0.0; DIMS], 3, 10)
        .await
        .unwrap_err()
        .to_string();
    assert!(corrupt.contains("corrupt"), "{corrupt}");
    assert!(missing.contains("store"), "{missing}");
    assert!(vector.contains("vector"), "{vector}");
    store
        .put(&data_key(), file(&vectors(9, 500)).into())
        .await
        .unwrap();
    let source = tagged
        .search(&store, &vs[0], 3, 10)
        .await
        .unwrap_err()
        .to_string();
    assert!(source.contains("source"), "{source}");
    // A bad tag flag is refused: the byte after the pin's three numbers.
    let mut bad = a.clone();
    let at = 4 + 4 + 8 + 4 + "lake/embeddings.bin".len() + 24;
    bad[at] = 7;
    assert!(ExternalIndex::from_bytes(&bad).is_err());
}

#[tokio::test]
async fn an_index_over_a_parquet_file_finds_the_exact_neighbours() {
    // `multipage.parquet` was written by pyarrow: 2,600 required FIXED_LEN_BYTE_ARRAY(32) rows
    // in several data pages. `multipage.vectors.f32` holds the floats the generator wrote.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let file = std::fs::read(format!("{dir}multipage.parquet")).unwrap();
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}multipage.vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    assert_eq!(all.len(), 2_600);
    let store = MemoryStore::new();
    let key = Key::new("lake/multipage.parquet");
    store.put(&key, file.clone().into()).await.unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert!(col.extents.len() > 1);
    let built = ExternalIndex::build(&store, &codes_key(), col, 700)
        .await
        .unwrap();
    let index = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    assert_eq!(
        built.to_bytes(),
        index.to_bytes(),
        "the runs survive the blob"
    );
    for qi in [0usize, 999, 1024, 2_599] {
        let q = &all[qi];
        let mut exact: Vec<(usize, f64)> = all
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(q, v)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        // A window over every row makes the rerank exact.
        let got = index.search(&store, q, 10, 2_600).await.unwrap();
        let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            want,
            "query {qi}"
        );
        assert_eq!(got[0].0, qi, "a row is its own nearest neighbour");
    }
    // A changed object is refused.
    let mut other = file;
    other[4] ^= 0xFF;
    store.put(&key, other.into()).await.unwrap();
    assert!(index.search(&store, &all[0], 3, 50).await.is_err());
    // Damage to the runs in the blob is refused: the first run's row count.
    let blob = built.to_bytes();
    let mut bad = blob.clone();
    // Walk to the first run's row count: magic, dims, rows, key, base/stride/len, the tag, the
    // run count, then `first_row`, then `rows`.
    let mut at = 4 + 4 + 8 + 4 + "lake/multipage.parquet".len() + 24;
    assert_eq!(blob[at], 1, "the memory store tags its objects");
    let taglen = u32::from_le_bytes(blob[at + 1..at + 5].try_into().unwrap()) as usize;
    at += 1 + 4 + taglen;
    let runs = u32::from_le_bytes(blob[at..at + 4].try_into().unwrap());
    assert_eq!(runs as usize, built.extent_count());
    at += 4 + 8;
    bad[at] ^= 0x01;
    assert!(
        ExternalIndex::from_bytes(&bad).is_err(),
        "runs that do not tile the rows"
    );
}

async fn parquet_index() -> (MemoryStore, ExternalIndex, Vec<Vec<f32>>) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let file = std::fs::read(format!("{dir}multipage.parquet")).unwrap();
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}multipage.vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    let store = MemoryStore::new();
    let key = Key::new("lake/multipage.parquet");
    store.put(&key, file.into()).await.unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col, 700)
        .await
        .unwrap();
    (store, index, all)
}

#[tokio::test]
async fn a_filter_prunes_before_the_window() {
    let (store, index, all) = parquet_index().await;
    let keep = |r: usize| r.is_multiple_of(5);
    let q = &all[1_000];
    let mut exact: Vec<(usize, f64)> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| keep(*i))
        .map(|(i, v)| (i, cosine(q, v)))
        .collect();
    exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
    let full = index
        .search_where(&store, q, 10, 2_600, &keep)
        .await
        .unwrap();
    assert_eq!(full.iter().map(|g| g.0).collect::<Vec<_>>(), want);
    assert!(full.iter().all(|g| keep(g.0)));
    // A window of 20 would hold about 4 survivors if filtered after; pruned first, it holds 20.
    let small = index.search_where(&store, q, 10, 20, &keep).await.unwrap();
    assert_eq!(small.len(), 10);
    assert!(small.iter().all(|g| keep(g.0)));
}

#[tokio::test]
async fn a_predicate_that_allows_nothing_reads_nothing() {
    let acct = Accounted::new(MemoryStore::new());
    let store = acct.as_tenant(TenantId(1));
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let key = Key::new("lake/multipage.parquet");
    store
        .put(
            &key,
            std::fs::read(format!("{dir}multipage.parquet"))
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col.clone(), 700)
        .await
        .unwrap();
    acct.record_ranges();
    let got = index
        .search_where(&store, &[1.0; 8], 10, 100, &|_| false)
        .await
        .unwrap();
    assert!(got.is_empty());
    assert_eq!(acct.bytes_in(&key, 0..col.len), 0);
}

#[tokio::test]
async fn search_is_search_where_allowing_all() {
    let (store, index, all) = parquet_index().await;
    assert_eq!(
        index.search(&store, &all[7], 10, 200).await.unwrap(),
        index
            .search_where(&store, &all[7], 10, 200, &|_| true)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_index_over_a_snappy_file_finds_the_exact_neighbours() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}multipage.vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    let store = MemoryStore::new();
    let key = Key::new("lake/ms.parquet");
    let file = std::fs::read(format!("{dir}multipage-snappy.parquet")).unwrap();
    store.put(&key, file.clone().into()).await.unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert!(col.extents.iter().all(|e| e.compressed > 0));
    let built = ExternalIndex::build(&store, &codes_key(), col, 700)
        .await
        .unwrap();
    let index = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    assert_eq!(
        built.to_bytes(),
        index.to_bytes(),
        "the compressed sizes survive the blob"
    );
    for qi in [0usize, 1_500, 2_599] {
        let q = &all[qi];
        let mut exact: Vec<(usize, f64)> = all
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(q, v)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let got = index.search(&store, q, 10, 2_600).await.unwrap();
        let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            want,
            "query {qi}"
        );
    }
    let mut other = file;
    other[4] ^= 0xFF;
    store.put(&key, other.into()).await.unwrap();
    assert!(index.search(&store, &all[0], 3, 50).await.is_err());
}
