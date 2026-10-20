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
async fn an_index_over_each_codec_finds_the_exact_neighbours() {
    for file in [
        "multipage-snappy",
        "multipage-gzip",
        "multipage-zstd",
        "multipage-brotli",
        "multipage-lz4raw",
        "multipage-v2",
        "multipage-v2-snappy",
        "multipage-v2-zstd",
        // M84: nullable columns that hold no nulls.
        "optional-multipage",
        "optional-v2",
        "optional-v2-zstd",
        // M91: lists of floats.
        "list-v1",
        "list-v2",
        "list-groups",
    ] {
        over_codec(file).await;
    }
    // M81: dictionary-encoded, plain / snappy / v2 (40 distinct vectors over 2,600 rows).
    for file in ["dictionary", "dictionary-snappy", "dictionary-v2"] {
        over_file(file, "dictionary.vectors.f32").await;
    }
}

async fn over_codec(name: &str) {
    over_file(name, "multipage.vectors.f32").await;
}

async fn over_file(name: &str, vectors: &str) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}{vectors}"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    let store = MemoryStore::new();
    let key = Key::new(format!("lake/{name}.parquet"));
    let file = std::fs::read(format!("{dir}{name}.parquet")).unwrap();
    store.put(&key, file.clone().into()).await.unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert!(col.extents.len() > 1, "{name}");
    let index = check_index(&store, col, &all).await;
    let mut other = file;
    other[4] ^= 0xFF;
    store.put(&key, other.into()).await.unwrap();
    assert!(index.search(&store, &all[0], 3, 50).await.is_err());
}

/// Builds the index over `col`, reopens it, and checks its top 10 against the exact cosine
/// ranking for three queries. Returns the reopened index.
async fn check_index(store: &MemoryStore, col: ExternalColumn, all: &[Vec<f32>]) -> ExternalIndex {
    let built = ExternalIndex::build(store, &codes_key(), col, 700)
        .await
        .unwrap();
    let index = ExternalIndex::open(store, &codes_key()).await.unwrap();
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
        let got = index.search(store, q, 10, 2_600).await.unwrap();
        let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            want,
            "query {qi}"
        );
    }
    index
}

#[tokio::test]
async fn an_index_over_a_lance_dataset_finds_the_exact_neighbours() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pstore-format/tests/data/lance/"
    );
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    let store = MemoryStore::new();
    let mut manifest = None;
    let root = format!("{dir}one");
    let mut stack = vec![std::path::PathBuf::from(&root)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            // Joined with `/`: on Windows `to_str()` gives `\`, and a key is never a
            // Windows path (M112).
            let rel = p
                .strip_prefix(&root)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/");
            let key = Key::new(format!("lake/one/{rel}"));
            store
                .put(&key, std::fs::read(&p).unwrap().into())
                .await
                .unwrap();
            if rel.ends_with(".manifest") {
                manifest = Some(key);
            }
        }
    }
    let manifest = manifest.unwrap();
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    let data_key = Key::new(col.key.clone());
    let index = check_index(&store, col, &all).await;
    // A changed data file is refused by the search.
    let mut other = std::fs::read(
        std::fs::read_dir(format!("{root}/data"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    other[4] ^= 0xFF;
    store.put(&data_key, other.into()).await.unwrap();
    assert!(index.search(&store, &all[0], 3, 50).await.is_err());
}

/// M88: a column with nulls (every row `i` with `i % 10 == 3`) keeps its row numbers, and no
/// search ever returns a null row.
#[tokio::test]
async fn an_index_over_a_column_with_nulls_finds_the_exact_neighbours() {
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
    let key = Key::new("lake/nulls.parquet");
    store
        .put(
            &key,
            std::fs::read(format!("{dir}optional-nulls.parquet"))
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert_eq!(col.rows, 2600);
    let built = ExternalIndex::build(&store, &codes_key(), col, 700)
        .await
        .unwrap();
    let index = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    assert_eq!(
        built.to_bytes(),
        index.to_bytes(),
        "the gaps survive the blob"
    );
    let is_null = |i: usize| i % 10 == 3;
    for qi in [0usize, 1_500, 2_599, 3, 2_593] {
        // A query that is itself a null row's vector still never returns that row.
        let q = &all[qi];
        let mut exact: Vec<(usize, f64)> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| !is_null(*i))
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
        assert!(got.iter().all(|g| !is_null(g.0)));
    }
    // And with a filter, which must not resurrect a null.
    let got = index
        .search_where(&store, &all[1], 10, 2_600, &|r| r % 2 == 1)
        .await
        .unwrap();
    assert!(got.iter().all(|g| g.0 % 2 == 1 && !is_null(g.0)));
    assert_eq!(got.len(), 10);
}

/// A Lance fixture dataset in a store, and its newest manifest (V2 naming counts down, so the
/// smallest name is the newest).
async fn lance_dataset(name: &str) -> (MemoryStore, Key) {
    lance_dataset_in("lance", name).await
}

/// As [`lance_dataset`], from the fixture directory `dir`.
async fn lance_dataset_in(dir: &str, name: &str) -> (MemoryStore, Key) {
    let root = format!(
        "{}/../pstore-format/tests/data/{dir}/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let store = MemoryStore::new();
    let mut manifest: Option<(String, Key)> = None;
    let mut stack = vec![std::path::PathBuf::from(&root)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            // Joined with `/`: on Windows `to_str()` gives `\`, and a key is never a
            // Windows path (M112).
            let rel = p
                .strip_prefix(&root)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/");
            let key = Key::new(format!("lake/{name}/{rel}"));
            store
                .put(&key, std::fs::read(&p).unwrap().into())
                .await
                .unwrap();
            if rel.ends_with(".manifest") && manifest.as_ref().is_none_or(|(r, _)| rel < *r) {
                manifest = Some((rel, key));
            }
        }
    }
    (store, manifest.unwrap().1)
}

fn lance_vectors() -> Vec<Vec<f32>> {
    std::fs::read(format!(
        "{}/../pstore-format/tests/data/lance/vectors.f32",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .chunks_exact(32)
    .map(|r| {
        r.chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect()
    })
    .collect()
}

/// M87: a dataset of three data files is one column, and its index finds the exact neighbours
/// across them.
#[tokio::test]
async fn an_index_over_a_multi_fragment_dataset_finds_the_exact_neighbours() {
    let (store, manifest) = lance_dataset("many").await;
    let all = lance_vectors();
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    assert_eq!(col.more.len(), 2);
    let last = Key::new(col.more.last().unwrap().key.clone());
    let index = check_index(&store, col, &all).await;
    // The blob carries the other data files: reopened, a changed one is noticed.
    let mut raw = store
        .get_range(&last, 0..store.head(&last).await.unwrap())
        .await
        .unwrap()
        .to_vec();
    raw[4] ^= 0xFF;
    store.put(&last, raw.into()).await.unwrap();
    assert!(index.search(&store, &all[0], 3, 50).await.is_err());
}

/// M87: deleted rows keep their numbers and are never returned.
#[tokio::test]
async fn an_index_over_a_dataset_with_deletions_never_returns_a_deleted_row() {
    let (store, manifest) = lance_dataset("deleted").await;
    let all = lance_vectors();
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    assert_eq!(col.rows, 300);
    let built = ExternalIndex::build(&store, &codes_key(), col, 100)
        .await
        .unwrap();
    let index = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    assert_eq!(built.to_bytes(), index.to_bytes());
    for qi in [0usize, 5, 150, 299] {
        let q = &all[qi];
        let mut exact: Vec<(usize, f64)> = (10..300).map(|i| (i, cosine(q, &all[i]))).collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let got = index.search(&store, q, 10, 300).await.unwrap();
        let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            want,
            "query {qi}"
        );
    }
}

/// M87: further objects without a CAS tag survive the codes blob too.
#[tokio::test]
async fn objects_without_a_tag_round_trip_through_the_blob() {
    let (store, manifest) = lance_dataset("many").await;
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col.clone(), 500)
        .await
        .unwrap();
    for tags in [
        (true, true, true),
        (false, false, false),
        (true, false, true),
        (false, true, false),
    ] {
        let mut c = col.clone();
        if !tags.0 {
            c.tag = None;
        }
        for (o, keep) in c.more.iter_mut().zip([tags.1, tags.2]) {
            if !keep {
                o.tag = None;
            }
        }
        let with = index.clone().with_column(c.clone());
        let back = ExternalIndex::from_bytes(&with.to_bytes()).unwrap();
        assert_eq!(back.to_bytes(), with.to_bytes(), "{tags:?}");
        assert_eq!(back.extent_count(), with.extent_count());
    }
}

// ---- M95: the attribute is a column of the same Parquet object ---------------------------------

use pstore_format::external::{AttrSource, AttrValue, ExternalAttribute};

fn id_of(v: Option<&AttrValue>) -> Option<i64> {
    match v {
        Some(AttrValue::Int(i)) => Some(*i),
        _ => None,
    }
}

#[tokio::test]
async fn a_filter_on_the_files_own_column_equals_the_exact_filtered_cosine() {
    let (store, index, all) = parquet_index().await;
    let key = Key::new("lake/multipage.parquet");
    let attr = ExternalAttribute::pin_parquet(&store, &key, "id")
        .await
        .unwrap();
    let keep = |v: Option<&AttrValue>| id_of(v).is_some_and(|i| i % 7 == 3);
    let q = &all[1_000];
    let mut exact: Vec<(usize, f64)> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 7 == 3)
        .map(|(i, v)| (i, cosine(q, v)))
        .collect();
    exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
    let full = index
        .search_attr(&store, q, 10, 2_600, &attr, &keep)
        .await
        .unwrap();
    assert_eq!(full.iter().map(|g| g.0).collect::<Vec<_>>(), want);
    assert!(full.iter().all(|g| g.0 % 7 == 3));
    // The same rows as the caller-supplied predicate of M73 admits.
    let by_hand = index
        .search_where(&store, q, 10, 2_600, &|r| r % 7 == 3)
        .await
        .unwrap();
    assert_eq!(full, by_hand);
    // A window of 20 would hold about 3 survivors if filtered after; pruned first, it holds 20.
    let small = index
        .search_attr(&store, q, 10, 20, &attr, &keep)
        .await
        .unwrap();
    assert_eq!(small.len(), 10);
    assert!(small.iter().all(|g| g.0 % 7 == 3));
}

#[tokio::test]
async fn an_attribute_of_another_object_or_length_is_refused() {
    let acct = Accounted::new(MemoryStore::new());
    let store = acct.as_tenant(TenantId(1));
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let put = |name: &'static str, file: &'static str| {
        let store = store.clone();
        async move {
            let key = Key::new(format!("lake/{name}"));
            let raw = std::fs::read(format!("{dir}{file}")).unwrap();
            store.put(&key, raw.into()).await.unwrap();
            key
        }
    };
    let key = put("multipage.parquet", "multipage.parquet").await;
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col, 700)
        .await
        .unwrap();
    // Another object (a copy of the very same file), pinned honestly.
    let other = put("elsewhere.parquet", "multipage.parquet").await;
    let attr = ExternalAttribute::pin_parquet(&store, &other, "id")
        .await
        .unwrap();
    // The right object name and a column of another length: 300 rows named as the 2,600.
    let short = put("short.parquet", "good.parquet").await;
    let mut wrong_len = ExternalAttribute::pin_parquet(&store, &short, "id")
        .await
        .unwrap();
    let AttrSource::Fixed(column) = &mut wrong_len.source else {
        unreachable!()
    };
    column.key = "lake/multipage.parquet".to_owned();
    acct.record_ranges();
    let err = index
        .search_attr(&store, &[1.0; 8], 10, 100, &attr, &|_| true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("another object"), "{err}");
    let err = index
        .search_attr(&store, &[1.0; 8], 10, 100, &wrong_len, &|_| true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rows"), "{err}");
    // Neither refusal moved a byte of any of the objects.
    for k in [&key, &other, &short] {
        assert_eq!(acct.bytes_in(k, 0..u64::MAX), 0, "{k:?}");
    }
}

#[tokio::test]
async fn an_attribute_that_admits_nothing_reads_no_vector() {
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
    let attr = ExternalAttribute::pin_parquet(&store, &key, "id")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col.clone(), 700)
        .await
        .unwrap();
    // The bytes the vectors occupy: the attribute's pages are elsewhere in the same file.
    let first = col.extents.iter().map(|e| e.offset).min().unwrap();
    let last = col
        .extents
        .iter()
        .map(|e| e.offset + e.rows * 32)
        .max()
        .unwrap();
    acct.record_ranges();
    let got = index
        .search_attr(&store, &[1.0; 8], 10, 100, &attr, &|_| false)
        .await
        .unwrap();
    assert!(got.is_empty());
    assert_eq!(acct.bytes_in(&key, first..last), 0);
    // The attribute itself was read: its pages are in the file, outside the vectors'.
    assert!(acct.bytes_in(&key, 0..col.len) > 0);
}

#[tokio::test]
async fn a_null_attribute_reaches_keep_as_none() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}good.vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    let store = MemoryStore::new();
    let key = Key::new("lake/vectors-attrs.parquet");
    store
        .put(
            &key,
            std::fs::read(format!("{dir}vectors-attrs.parquet"))
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col, 100)
        .await
        .unwrap();
    let grp = ExternalAttribute::pin_parquet(&store, &key, "grp")
        .await
        .unwrap();
    let score = ExternalAttribute::pin_parquet(&store, &key, "score")
        .await
        .unwrap();
    let null = |r: usize| r % 7 == 3;
    let exact = |keep: &dyn Fn(usize) -> bool| -> Vec<usize> {
        let q = &all[10];
        let mut e: Vec<(usize, f64)> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| keep(*i))
            .map(|(i, v)| (i, cosine(q, v)))
            .collect();
        e.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        e.iter().take(10).map(|x| x.0).collect()
    };
    let rows = |hits: Vec<(usize, f32)>| hits.into_iter().map(|h| h.0).collect::<Vec<_>>();
    // Only the nulls.
    let got = index
        .search_attr(&store, &all[10], 10, 300, &grp, &|v| v.is_none())
        .await
        .unwrap();
    assert_eq!(rows(got), exact(&null));
    // Everything but the nulls, and the value is the file's.
    let got = index
        .search_attr(
            &store,
            &all[10],
            10,
            300,
            &grp,
            &|v| matches!(v, Some(AttrValue::Int(g)) if *g == 2),
        )
        .await
        .unwrap();
    assert_eq!(rows(got), exact(&|r| !null(r) && r % 5 == 2));
    // A float attribute is compared as the number it is.
    let got = index
        .search_attr(&store, &all[10], 10, 300, &score, &|v| {
            v.is_some_and(|v| v.as_f64() >= 29.0)
        })
        .await
        .unwrap();
    assert_eq!(rows(got), exact(&|r| r >= 290));
    // Null is not admitted by a predicate that wants a value.
    let got = index
        .search_attr(&store, &all[10], 10, 300, &grp, &|v| v.is_some())
        .await
        .unwrap();
    assert_eq!(rows(got), exact(&|r| !null(r)));
}

// ---- M96: clustered codes ----

const LISTS: usize = 40;

async fn clustered(store: &MemoryStore, vs: &[Vec<f32>]) -> ExternalIndex {
    store.put(&data_key(), file(vs).into()).await.unwrap();
    let col = column(store, vs.len()).await;
    ExternalIndex::build_clustered(store, &codes_key(), col, 1_024, LISTS)
        .await
        .unwrap()
}

fn queries(vs: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let mut s = 99;
    (0..20)
        .map(|_| {
            vs[(rnd(&mut s).abs() * 19_000.0) as usize]
                .iter()
                .map(|x| x + 0.3 * rnd(&mut s))
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn a_clustered_blob_round_trips_and_refuses_damage() {
    let vs = vectors(5, ROWS);
    let store = MemoryStore::new();
    let built = clustered(&store, &vs).await;
    let nlist = built.cluster_count();
    assert!(nlist > 1 && nlist <= LISTS, "{nlist}");
    let flat = {
        let col = column(&store, ROWS).await;
        ExternalIndex::build(&store, &Key::new("t/ext/flat"), col, 1_024)
            .await
            .unwrap()
    };
    let blob = built.to_bytes();
    assert_eq!(
        blob.len(),
        flat.to_bytes().len() + 4 + nlist * DIMS * 4 + ROWS * 4
    );
    let opened = ExternalIndex::open(&store, &codes_key()).await.unwrap();
    assert_eq!(opened.to_bytes(), blob);
    let q = &queries(&vs)[0];
    assert_eq!(
        opened.search_probes(&store, q, 10, 200, 3).await.unwrap(),
        built.search_probes(&store, q, 10, 200, 3).await.unwrap()
    );
    // Every row sits in exactly one list: all the lists together are every row, once.
    assert_eq!(
        opened.probed_rows(q, nlist).unwrap(),
        (0..ROWS).collect::<Vec<_>>()
    );
    // An unknown magic is refused even when the rest parses as a clustered blob.
    let mut alien = blob.clone();
    alien[..4].copy_from_slice(b"PXI9");
    assert!(ExternalIndex::from_bytes(&alien).is_err());
    // Damage: a list id out of range, a truncated section, a trailing byte.
    let mut bad = blob.clone();
    let n = bad.len();
    bad[n - 4..].copy_from_slice(&(nlist as u32).to_le_bytes());
    assert!(ExternalIndex::from_bytes(&bad).is_err());
    assert!(ExternalIndex::from_bytes(&blob[..n - 1]).is_err());
    // A hostile list count: zero, and one the blob cannot hold.
    let at = n - (4 + nlist * DIMS * 4 + ROWS * 4);
    for hostile in [0u32, u32::MAX, nlist as u32 + 1] {
        let mut h = blob.clone();
        h[at..at + 4].copy_from_slice(&hostile.to_le_bytes());
        assert!(ExternalIndex::from_bytes(&h).is_err(), "nlist {hostile}");
    }
    let mut long = blob.clone();
    long.push(0);
    assert!(ExternalIndex::from_bytes(&long).is_err());
    // A flat blob still opens, has no clusters, and answers as it did.
    let reopened = ExternalIndex::from_bytes(&flat.to_bytes()).unwrap();
    assert_eq!(reopened.cluster_count(), 0);
    assert_eq!(
        reopened.search(&store, q, 10, 200).await.unwrap(),
        flat.search(&store, q, 10, 200).await.unwrap()
    );
}

#[tokio::test]
async fn probing_every_list_is_the_flat_search() {
    let vs = vectors(5, ROWS);
    let store = MemoryStore::new();
    let index = clustered(&store, &vs).await;
    let nlist = index.cluster_count();
    for q in queries(&vs) {
        let flat = index.search(&store, &q, 10, 200).await.unwrap();
        assert_eq!(
            index
                .search_probes(&store, &q, 10, 200, nlist)
                .await
                .unwrap(),
            flat
        );
        assert_eq!(
            index
                .search_probes(&store, &q, 10, 200, nlist + 5)
                .await
                .unwrap(),
            flat
        );
    }
}

async fn recall_at(
    index: &ExternalIndex,
    store: &MemoryStore,
    vs: &[Vec<f32>],
    probes: usize,
) -> f64 {
    let mut hit = 0usize;
    for q in queries(vs) {
        let mut exact: Vec<(usize, f64)> = vs
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(&q, v)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let got = index
            .search_probes(store, &q, 10, 200, probes)
            .await
            .unwrap();
        hit += exact[..10]
            .iter()
            .filter(|e| got.iter().any(|g| g.0 == e.0))
            .count();
    }
    hit as f64 / 200.0
}

#[tokio::test]
async fn a_few_probes_find_the_neighbours_and_score_a_fraction_of_the_rows() {
    const FLOOR: f64 = 0.9;
    let vs = vectors(5, ROWS);
    let store = MemoryStore::new();
    let index = clustered(&store, &vs).await;
    let four = recall_at(&index, &store, &vs, 4).await;
    let one = recall_at(&index, &store, &vs, 1).await;
    eprintln!("recall probes=4 {four} probes=1 {one}");
    assert!(four >= FLOOR, "recall {four}");
    assert!(one <= four, "{one} > {four}");
    for q in queries(&vs) {
        let scored = index.probed_rows(&q, 4).unwrap().len();
        assert!(
            scored > 0 && scored * 4 <= ROWS,
            "scored {scored} of {ROWS}"
        );
    }
}

#[tokio::test]
async fn a_filter_composes_with_the_probes() {
    let vs = vectors(5, ROWS);
    let store = MemoryStore::new();
    let index = clustered(&store, &vs).await;
    let nlist = index.cluster_count();
    for q in queries(&vs).iter().take(5) {
        let got = index
            .search_where_probes(&store, q, 10, 200, &|r| r % 3 == 1, 4)
            .await
            .unwrap();
        assert!(!got.is_empty() && got.iter().all(|g| g.0 % 3 == 1));
        assert_eq!(
            index
                .search_where_probes(&store, q, 10, 200, &|r| r % 3 == 1, nlist)
                .await
                .unwrap(),
            index
                .search_where(&store, q, 10, 200, &|r| r % 3 == 1)
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn probes_of_zero_and_an_unclustered_index_are_refused() {
    let vs = vectors(5, 2_000);
    let store = MemoryStore::new();
    let index = clustered(&store, &vs).await;
    let q = &vs[3];
    assert!(index.search_probes(&store, q, 10, 100, 0).await.is_err());
    let col = column(&store, 2_000).await;
    let flat = ExternalIndex::build(&store, &Key::new("t/ext/flat"), col, 512)
        .await
        .unwrap();
    assert!(flat.search_probes(&store, q, 10, 100, 2).await.is_err());
    let col = column(&store, 2_000).await;
    assert!(
        ExternalIndex::build_clustered(&store, &codes_key(), col, 512, 0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_column_with_nulls_is_clustered() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let store = MemoryStore::new();
    let key = Key::new("lake/nulls.parquet");
    store
        .put(
            &key,
            std::fs::read(format!("{dir}optional-nulls.parquet"))
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let index = ExternalIndex::build_clustered(&store, &codes_key(), col, 700, 8)
        .await
        .unwrap();
    let nlist = index.cluster_count();
    let is_null = |i: usize| i % 10 == 3;
    let vecs: Vec<Vec<f32>> = std::fs::read(format!("{dir}multipage.vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    for qi in [0usize, 1_500, 3] {
        let q = &vecs[qi];
        let flat = index.search(&store, q, 10, 2_600).await.unwrap();
        let probed = index
            .search_probes(&store, q, 10, 2_600, nlist)
            .await
            .unwrap();
        assert_eq!(probed, flat);
        let few = index.search_probes(&store, q, 10, 2_600, 2).await.unwrap();
        assert!(few.iter().all(|g| !is_null(g.0)));
        assert!(
            !index
                .probed_rows(q, nlist)
                .unwrap()
                .iter()
                .any(|r| is_null(*r))
        );
    }
}

// ---- M99: a timestamp attribute ---------------------------------------------------------------

#[tokio::test]
async fn a_filter_on_a_timestamp_column_equals_the_exact_filtered_cosine() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}good.vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    let store = MemoryStore::new();
    let key = Key::new("lake/vectors-ts.parquet");
    let raw = std::fs::read(format!("{dir}vectors-ts.parquet")).unwrap();
    store.put(&key, raw.into()).await.unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col, 100)
        .await
        .unwrap();
    let ts = ExternalAttribute::pin_parquet(&store, &key, "ts")
        .await
        .unwrap();
    // `ts` is r x 10^6 ms: rows 150 to 299 are at or after 150,000,000,000 us.
    let keep =
        |v: Option<&AttrValue>| matches!(v, Some(AttrValue::DateTime(t)) if *t >= 150_000_000_000);
    let q = &all[10];
    let mut exact: Vec<(usize, f64)> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= 150)
        .map(|(i, v)| (i, cosine(q, v)))
        .collect();
    exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
    let got = index
        .search_attr(&store, q, 10, 300, &ts, &keep)
        .await
        .unwrap();
    assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), want);
}

#[tokio::test]
async fn a_filter_on_a_string_column_equals_the_exact_filtered_cosine() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
    let all: Vec<Vec<f32>> = std::fs::read(format!("{dir}good.vectors.f32"))
        .unwrap()
        .chunks_exact(32)
        .map(|r| {
            r.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        })
        .collect();
    let store = MemoryStore::new();
    let key = Key::new("lake/vectors-labels.parquet");
    let raw = std::fs::read(format!("{dir}vectors-labels.parquet")).unwrap();
    store.put(&key, raw.into()).await.unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col, 100)
        .await
        .unwrap();
    let q = &all[10];
    // `label` is "odd" on odd rows and null every 7th; `flag` is set on every third row.
    let odd = |i: usize| i % 2 == 1 && i % 7 != 3;
    let third = |i: usize| i.is_multiple_of(3);
    let label = ExternalAttribute::pin_parquet(&store, &key, "label")
        .await
        .unwrap();
    let flag = ExternalAttribute::pin_parquet(&store, &key, "flag")
        .await
        .unwrap();
    let keep_odd = |v: Option<&AttrValue>| matches!(v, Some(AttrValue::Str(s)) if s == "odd");
    let keep_flag = |v: Option<&AttrValue>| matches!(v, Some(AttrValue::Bool(true)));
    for (attr, keep, rows) in [
        (
            &label,
            &keep_odd as &(dyn Fn(Option<&AttrValue>) -> bool + Sync),
            &odd as &dyn Fn(usize) -> bool,
        ),
        (&flag, &keep_flag, &third),
    ] {
        let mut exact: Vec<(usize, f64)> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| rows(*i))
            .map(|(i, v)| (i, cosine(q, v)))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
        let got = index
            .search_attr(&store, q, 10, 300, attr, keep)
            .await
            .unwrap();
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), want);
    }
}

/// M101: a filter on a Lance dataset's own scalar column, across its data files and deletions.
#[tokio::test]
async fn a_filter_on_a_lance_column_equals_the_exact_filtered_cosine() {
    let (store, manifest) = lance_dataset_in("lance-scalars", "many").await;
    let all = lance_vectors();
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    let index = ExternalIndex::build(&store, &codes_key(), col, 100)
        .await
        .unwrap();
    let attr = ExternalAttribute::pin_lance(&store, &manifest, "int64")
        .await
        .unwrap();
    // `int64` is (r - 1500) << 40, so this keeps rows 900 and on; 1000 to 1009 are deleted.
    let keep = |v: Option<&AttrValue>| matches!(v, Some(AttrValue::Int(x)) if *x >= -600 << 40);
    let kept = |i: usize| i >= 900 && !(1000..1010).contains(&i);
    for qi in [0usize, 1005, 2000] {
        let q = &all[qi];
        let mut exact: Vec<(usize, f64)> = (0..2600)
            .filter(|i| kept(*i))
            .map(|i| (i, cosine(q, &all[i])))
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let want: Vec<usize> = exact.iter().take(10).map(|e| e.0).collect();
        let got = index
            .search_attr(&store, q, 10, 2600, &attr, &keep)
            .await
            .unwrap();
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            want,
            "query {qi}"
        );
    }
    // An attribute whose second data file is another object is refused before a vector moves.
    let mut moved = attr.clone();
    let AttrSource::Fixed(column) = &mut moved.source else {
        unreachable!()
    };
    column.more[0].key.push('x');
    let err = index
        .search_attr(&store, &all[0], 10, 2600, &moved, &keep)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("another object"), "{err}");
}
