//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M70: a vector column found in real Parquet files' footers. The fixtures in `tests/data` were
//! written by pyarrow 26; `*.vectors.f32` holds the floats the generator wrote.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::external::{Extent, ExternalColumn, ExternalError};
use pstore_format::parquet::{ParquetError, locate};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/");

fn bytes(name: &str) -> Vec<u8> {
    std::fs::read(format!("{DIR}{name}")).unwrap()
}

async fn put(name: &str) -> (MemoryStore, Key) {
    let store = MemoryStore::new();
    let key = Key::new(format!("lake/{name}"));
    store.put(&key, bytes(name).into()).await.unwrap();
    (store, key)
}

fn floats(name: &str) -> Vec<f32> {
    bytes(name)
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

async fn check_every_row(name: &str, vectors: &str, rows: usize, pages: usize) {
    let (store, key) = put(name).await;
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert_eq!((col.dims, col.rows), (8, rows as u64));
    assert_eq!(col.extents.len(), pages, "{name}: pages");
    let want = floats(vectors);
    let all: Vec<usize> = (0..rows).collect();
    let got = col.read_rows(&store, &all).await.unwrap();
    assert_eq!(got.len(), rows);
    for (r, v) in &got {
        assert_eq!(v.as_slice(), &want[r * 8..r * 8 + 8], "{name} row {r}");
    }
    assert!(col.row_range(rows).is_none());
}

#[tokio::test]
async fn locate_finds_the_real_files_vectors() {
    // Three row groups of 100 rows, one page each.
    check_every_row("good.parquet", "good.vectors.f32", 300, 3).await;
    // One row group of 2,600 rows split across several pages.
    let (store, key) = put("multipage.parquet").await;
    let found = locate(&store, &key, "embedding").await.unwrap();
    assert!(found.extents.len() > 1, "{:?}", found.extents);
    check_every_row(
        "multipage.parquet",
        "multipage.vectors.f32",
        2600,
        found.extents.len(),
    )
    .await;
    // The id column is INT64, not a vector.
    let err = locate(&store, &key, "id").await.unwrap_err();
    assert!(err.to_string().contains("FIXED_LEN_BYTE_ARRAY"), "{err}");
}

#[tokio::test]
async fn locate_refuses_what_cannot_be_reached() {
    for (file, why) in [("list.parquet", "repetition levels")] {
        let (store, key) = put(file).await;
        let err = locate(&store, &key, "embedding").await.unwrap_err();
        assert!(matches!(err, ParquetError::Unsupported(_)), "{file}: {err}");
        assert!(err.to_string().contains(why), "{file}: {err}");
    }
    let (store, key) = put("good.parquet").await;
    let err = locate(&store, &key, "nope").await.unwrap_err();
    assert!(err.to_string().contains("no column"), "{err}");

    let good = bytes("good.parquet");
    let refuse = |name: &'static str, body: Vec<u8>| {
        let store = store.clone();
        async move {
            let k = Key::new(format!("lake/bad-{name}"));
            store.put(&k, body.into()).await.unwrap();
            locate(&store, &k, "embedding").await
        }
    };
    assert!(refuse("short", good[..4].to_vec()).await.is_err());
    assert!(refuse("empty", Vec::new()).await.is_err());
    assert!(
        refuse("text", b"definitely not parquet, just words".to_vec())
            .await
            .is_err()
    );
    assert!(
        refuse("trunc", good[..good.len() - 20].to_vec())
            .await
            .is_err()
    );
    // A footer length larger than the file.
    let mut huge = good.clone();
    let n = huge.len();
    huge[n - 8..n - 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        refuse("huge", huge).await,
        Err(ParquetError::Malformed(_))
    ));
    // A zero footer length.
    let mut zero = good.clone();
    zero[n - 8..n - 4].copy_from_slice(&0u32.to_le_bytes());
    assert!(refuse("zero", zero).await.is_err());
    // A damaged footer: flip bytes through it; none may panic, and the file is refused or,
    // where the flip is harmless, still located. Every outcome is an answer.
    let size = u32::from_le_bytes(good[n - 8..n - 4].try_into().unwrap()) as usize;
    let mut refused = 0;
    for at in (n - 8 - size..n - 8).step_by(3) {
        let mut b = good.clone();
        b[at] ^= 0xFF;
        if refuse("flip", b).await.is_err() {
            refused += 1;
        }
    }
    assert!(refused > 10, "{refused} flips refused");
    // Bad magic.
    let mut magic = good.clone();
    magic[n - 1] = b'X';
    assert!(matches!(
        refuse("magic", magic).await,
        Err(ParquetError::Malformed(_))
    ));
}

#[tokio::test]
async fn extents_address_rows() {
    let col = ExternalColumn::from_extents(
        "k",
        2,
        1000,
        None,
        vec![
            Extent {
                first_row: 0,
                rows: 3,
                offset: 100,
                compressed: 0,
                codec: 0,
                plain_len: 0,
                dict: None,
            },
            Extent {
                first_row: 3,
                rows: 1,
                offset: 200,
                compressed: 0,
                codec: 0,
                plain_len: 0,
                dict: None,
            },
            Extent {
                first_row: 4,
                rows: 5,
                offset: 300,
                compressed: 0,
                codec: 0,
                plain_len: 0,
                dict: None,
            },
        ],
    )
    .unwrap();
    assert_eq!(col.rows, 9);
    let r = |i: usize| col.row_range(i);
    assert_eq!(r(0), Some(100..108));
    assert_eq!(r(2), Some(116..124));
    assert_eq!(r(3), Some(200..208));
    assert_eq!(r(4), Some(300..308));
    assert_eq!(r(8), Some(332..340));
    assert_eq!(r(9), None);
    let shape =
        |extents: Vec<Extent>, len: u64| ExternalColumn::from_extents("k", 2, len, None, extents);
    let e = |first_row, rows, offset| Extent {
        compressed: 0,
        codec: 0,
        plain_len: 0,
        dict: None,
        first_row,
        rows,
        offset,
    };
    assert!(shape(vec![], 100).is_err(), "none");
    assert!(shape(vec![e(0, 0, 0)], 100).is_err(), "zero rows");
    assert!(shape(vec![e(0, 2, 0), e(3, 1, 16)], 100).is_err(), "gap");
    assert!(
        shape(vec![e(0, 2, 0), e(1, 2, 16)], 100).is_err(),
        "overlap"
    );
    assert!(shape(vec![e(1, 2, 0)], 100).is_err(), "not from 0");
    assert!(shape(vec![e(0, 2, 90)], 100).is_err(), "past the object");
    assert!(
        shape(vec![e(0, 2, u64::MAX - 4)], u64::MAX).is_err(),
        "overflow"
    );
    assert!(shape(vec![e(0, 2, 85)], 100).is_err(), "one byte past");
    assert!(shape(vec![e(0, 2, 84)], 100).is_ok(), "exact fit");
    assert!(ExternalColumn::from_extents("k", 0, 100, None, vec![e(0, 1, 0)]).is_err());

    // A changed object is refused for a located column, as for a single run.
    let (store, key) = put("good.parquet").await;
    let pinned = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert!(pinned.read_rows(&store, &[5]).await.is_ok());
    let mut other = bytes("good.parquet");
    other[4] ^= 0xFF;
    store.put(&key, other.into()).await.unwrap();
    assert!(matches!(
        pinned.read_rows(&store, &[5]).await,
        Err(ExternalError::Changed)
    ));
}

// ---- A tiny Parquet writer, to build the shapes pyarrow never produces ----------------------

/// Thrift compact output.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8 & 0x7F) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn zig(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, last: &mut i16, id: i16, ty: u8) {
        let delta = id - *last;
        if (1..=15).contains(&delta) {
            self.0.push((delta as u8) << 4 | ty);
        } else {
            self.0.push(ty);
            self.zig(i64::from(id));
        }
        *last = id;
    }
    fn i32(&mut self, last: &mut i16, id: i16, v: i64) {
        self.field(last, id, 5);
        self.zig(v);
    }
    fn i64(&mut self, last: &mut i16, id: i16, v: i64) {
        self.field(last, id, 6);
        self.zig(v);
    }
    fn bin(&mut self, last: &mut i16, id: i16, v: &[u8]) {
        self.field(last, id, 8);
        self.varint(v.len() as u64);
        self.0.extend(v);
    }
    fn list(&mut self, last: &mut i16, id: i16, elem: u8, n: usize) {
        self.field(last, id, 9);
        if n < 15 {
            self.0.push((n as u8) << 4 | elem);
        } else {
            self.0.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }
    fn stop(&mut self) {
        self.0.push(0);
    }
}

/// One synthetic data page: header fields and the number of body bytes actually written.
#[derive(Clone)]
struct Page {
    values: i64,
    compressed: i64,
    uncompressed: i64,
    encoding: i64,
    kind: i64,
    body: usize,
    /// A v2 header's `(nulls, definition length, repetition length, is_compressed)`.
    v2: Option<(i64, i64, i64, bool)>,
}

/// One column chunk, as its metadata claims.
#[derive(Clone)]
struct Chunk {
    pages: Vec<Page>,
    num_values: i64,
    total_compressed: Option<i64>,
    codec: i64,
    encodings: Vec<i64>,
    dictionary_offset: bool,
    /// Records a dictionary page offset of 0, as some writers do for "none".
    dictionary_zero: bool,
    chunk_type: i64,
    group_rows: i64,
}

#[derive(Clone)]
struct Spec {
    type_length: i64,
    column_type: i64,
    repetition: i64,
    chunks: Vec<Chunk>,
    path: &'static str,
    extra_struct_field: bool,
}

fn page(dims: usize, values: i64) -> Page {
    let bytes = values * dims as i64 * 4;
    Page {
        values,
        compressed: bytes,
        uncompressed: bytes,
        encoding: 0,
        kind: 0,
        body: bytes as usize,
        v2: None,
    }
}

fn chunk(dims: usize, rows: &[i64]) -> Chunk {
    let pages: Vec<Page> = rows.iter().map(|r| page(dims, *r)).collect();
    let n = rows.iter().sum();
    Chunk {
        pages,
        num_values: n,
        total_compressed: None,
        codec: 0,
        encodings: vec![0, 3],
        dictionary_offset: false,
        dictionary_zero: false,
        chunk_type: 7,
        group_rows: n,
    }
}

fn spec(dims: usize, chunks: Vec<Chunk>) -> Spec {
    Spec {
        type_length: dims as i64 * 4,
        column_type: 7,
        repetition: 0,
        chunks,
        path: "v",
        extra_struct_field: false,
    }
}

/// The file's bytes, and each chunk's first page offset.
fn build(s: &Spec) -> Vec<u8> {
    let mut file = b"PAR1".to_vec();
    let mut chunk_meta = Vec::new();
    for c in &s.chunks {
        let start = file.len();
        for p in &c.pages {
            let mut h = Out::default();
            let mut l = 0;
            h.i32(&mut l, 1, p.kind);
            h.i32(&mut l, 2, p.uncompressed);
            h.i32(&mut l, 3, p.compressed);
            if let Some((nulls, dl, rl, comp)) = p.v2 {
                h.field(&mut l, 8, 12);
                let mut d = 0;
                h.i32(&mut d, 1, p.values);
                h.i32(&mut d, 2, nulls);
                h.i32(&mut d, 3, p.values);
                h.i32(&mut d, 4, p.encoding);
                h.i32(&mut d, 5, dl);
                h.i32(&mut d, 6, rl);
                h.field(&mut d, 7, if comp { 1 } else { 2 });
                h.stop();
                h.stop();
            } else {
                h.field(&mut l, 5, 12);
                let mut d = 0;
                h.i32(&mut d, 1, p.values);
                h.i32(&mut d, 2, p.encoding);
                h.i32(&mut d, 3, 3);
                h.i32(&mut d, 4, 3);
                h.stop();
                h.stop();
            }
            file.extend(&h.0);
            file.extend(std::iter::repeat_n(0x5A, p.body));
        }
        let size = (file.len() - start) as i64;
        chunk_meta.push((start as i64, c.total_compressed.unwrap_or(size)));
    }
    let mut f = Out::default();
    let mut l = 0;
    f.i32(&mut l, 1, 1);
    f.list(&mut l, 2, 12, 2);
    // root
    let mut e = 0;
    f.bin(&mut e, 4, b"schema");
    f.i32(&mut e, 5, 1);
    f.stop();
    // the column
    let mut e = 0;
    f.i32(&mut e, 1, s.column_type);
    f.i32(&mut e, 2, s.type_length);
    f.i32(&mut e, 3, s.repetition);
    f.bin(&mut e, 4, s.path.as_bytes());
    f.stop();
    f.i64(&mut l, 3, s.chunks.iter().map(|c| c.group_rows).sum());
    f.list(&mut l, 4, 12, s.chunks.len());
    for (c, (start, total)) in s.chunks.iter().zip(chunk_meta) {
        let mut g = 0;
        f.list(&mut g, 1, 12, 1);
        let mut cc = 0;
        f.i64(&mut cc, 2, 0);
        f.field(&mut cc, 3, 12);
        let mut m = 0;
        f.i32(&mut m, 1, c.chunk_type);
        f.list(&mut m, 2, 5, c.encodings.len());
        for e in &c.encodings {
            f.zig(*e);
        }
        f.list(&mut m, 3, 8, 1);
        f.varint(s.path.len() as u64);
        f.0.extend(s.path.as_bytes());
        f.i32(&mut m, 4, c.codec);
        f.i64(&mut m, 5, c.num_values);
        f.i64(&mut m, 7, total);
        f.i64(&mut m, 9, start);
        if c.dictionary_offset {
            f.i64(&mut m, 11, start);
        } else if c.dictionary_zero {
            f.i64(&mut m, 11, 0);
        }
        f.stop();
        if s.extra_struct_field {
            // A struct-typed field after the metadata that is not field 3.
            f.field(&mut cc, 4, 12);
            let mut x = 0;
            f.i32(&mut x, 1, 99);
            f.stop();
        }
        f.stop();
        f.i64(&mut g, 3, c.group_rows);
        f.stop();
    }
    f.stop();
    let footer = f.0;
    file.extend(&footer);
    file.extend((footer.len() as u32).to_le_bytes());
    file.extend(b"PAR1");
    file
}

async fn run(s: &Spec) -> Result<pstore_format::parquet::Located, ParquetError> {
    let store = MemoryStore::new();
    let key = Key::new("lake/synth.parquet");
    store.put(&key, build(s).into()).await.unwrap();
    locate(&store, &key, "v").await
}

async fn refused(s: &Spec, why: &str) {
    let err = run(s).await.unwrap_err();
    assert!(err.to_string().contains(why), "wanted {why:?}, got {err}");
}

#[tokio::test]
async fn the_writer_builds_what_locate_reads() {
    let s = spec(2, vec![chunk(2, &[3, 2]), chunk(2, &[4])]);
    let found = run(&s).await.unwrap();
    assert_eq!((found.dims, found.extents.len()), (2, 3));
    let rows: Vec<(u64, u64)> = found
        .extents
        .iter()
        .map(|e| (e.first_row, e.rows))
        .collect();
    assert_eq!(rows, [(0, 3), (3, 2), (5, 4)]);
    // Each page's body follows its header, and the pages follow one another.
    for w in found.extents.windows(2) {
        assert!(w[1].offset > w[0].offset + w[0].rows * 8);
    }
    // A struct field the reader does not know, after the metadata, is skipped.
    let mut extra = s.clone();
    extra.extra_struct_field = true;
    assert_eq!(run(&extra).await.unwrap().extents, found.extents);
    // A different column name in the file is simply not that column.
    let mut other = s.clone();
    other.path = "w";
    refused(&other, "no column").await;
}

#[tokio::test]
async fn locate_refuses_each_shape_it_cannot_read() {
    let base = spec(4, vec![chunk(4, &[5])]);
    let with = |f: &dyn Fn(&mut Spec)| {
        let mut s = base.clone();
        f(&mut s);
        s
    };
    refused(&with(&|s| s.type_length = 6), "whole number").await;
    refused(&with(&|s| s.type_length = 0), "whole number").await;
    refused(&with(&|s| s.type_length = -4), "whole number").await;
    refused(&with(&|s| s.column_type = 1), "FIXED_LEN_BYTE_ARRAY").await;
    // OPTIONAL (1) pages are checked for levels, which these synthetic pages do not have;
    // REPEATED (2) is never read.
    refused(&with(&|s| s.repetition = 1), "definition levels").await;
    refused(&with(&|s| s.repetition = 2), "REPEATED").await;
    refused(&with(&|s| s.chunks[0].codec = 3), "compressed with codec").await;
    refused(
        &with(&|s| s.chunks[0].codec = 5),
        "only snappy, gzip, brotli, lz4_raw and zstd",
    )
    .await;
    refused(
        &with(&|s| s.chunks[0].dictionary_offset = true),
        "dictionary page out of place",
    )
    .await;
    // An offset of 0 means no dictionary: the chunk reads as any other.
    run(&with(&|s| s.chunks[0].dictionary_zero = true))
        .await
        .unwrap();
    refused(&with(&|s| s.chunks[0].encodings = vec![0, 5]), "PLAIN").await;
    refused(&with(&|s| s.chunks[0].encodings = vec![9]), "PLAIN").await;
    refused(&with(&|s| s.chunks[0].chunk_type = 1), "disagrees").await;
    refused(&with(&|s| s.chunks[0].pages[0].encoding = 5), "PLAIN").await;
    // Indices into a dictionary the chunk never declared.
    refused(
        &with(&|s| s.chunks[0].pages[0].encoding = 2),
        "before any dictionary",
    )
    .await;
    refused(
        &with(&|s| s.chunks[0].pages[0].encoding = 8),
        "before any dictionary",
    )
    .await;
    // A page that is compressed, or carries levels, is not `values × width` bytes.
    refused(&with(&|s| s.chunks[0].pages[0].compressed -= 4), "levels").await;
    refused(&with(&|s| s.chunks[0].pages[0].uncompressed += 4), "levels").await;
    refused(
        &with(&|s| {
            s.chunks[0].pages[0].compressed += 4;
            s.chunks[0].pages[0].uncompressed += 4;
        }),
        "levels",
    )
    .await;
    // A dictionary page the chunk never declared, and a page type that is neither.
    refused(
        &with(&|s| s.chunks[0].pages[0].kind = 2),
        "dictionary page out of place",
    )
    .await;
    refused(&with(&|s| s.chunks[0].pages[0].kind = 3), "page header").await;
    // Counts that disagree.
    refused(&with(&|s| s.chunks[0].group_rows = 6), "one a row").await;
    refused(&with(&|s| s.chunks[0].num_values = 6), "one a row").await;
    refused(
        &with(&|s| s.chunks[0].total_compressed = Some(1 << 40)),
        "past the file",
    )
    .await;
    // The chunk is shorter than its page claims.
    refused(
        &with(&|s| s.chunks[0].total_compressed = Some(30)),
        "past its chunk",
    )
    .await;
    // A page of no values.
    refused(
        &with(&|s| {
            s.chunks[0] = chunk(4, &[0, 5]);
        }),
        "past its chunk",
    )
    .await;
    // Fewer values in the pages than the chunk claims: the pages run out.
    let mut short = base.clone();
    short.chunks[0].num_values = 8;
    short.chunks[0].group_rows = 8;
    refused(&short, "end before").await;
    // More values in the pages than the chunk claims.
    let mut over = spec(4, vec![chunk(4, &[3, 3])]);
    over.chunks[0].num_values = 4;
    over.chunks[0].group_rows = 4;
    refused(&over, "overshoot").await;
    // A page body that is missing: the header's claim runs past the chunk.
    let mut cut = base.clone();
    cut.chunks[0].pages[0].body -= 8;
    refused(&cut, "past its chunk").await;
}

#[tokio::test]
async fn a_footer_that_is_all_there_is() {
    // No row groups: a file of magic, footer, length, magic. It locates no extents.
    let mut s = spec(2, vec![]);
    s.chunks.clear();
    let found = run(&s).await.unwrap();
    assert!(found.extents.is_empty());
    assert_eq!(found.len as usize, build(&s).len());
    // A footer length that leaves no room for the leading magic is refused as out of range.
    let store = MemoryStore::new();
    let key = Key::new("lake/edge.parquet");
    let good = build(&s);
    let n = good.len();
    let fit = |extra: u32| {
        let mut b = good.clone();
        let size = u32::from_le_bytes(b[n - 8..n - 4].try_into().unwrap());
        b[n - 8..n - 4].copy_from_slice(&(size + extra).to_le_bytes());
        b
    };
    for extra in [1u32, 2, 4] {
        store.put(&key, fit(extra).into()).await.unwrap();
        let err = locate(&store, &key, "v").await.unwrap_err();
        assert!(err.to_string().contains("out of range"), "{extra}: {err}");
    }
}

// ---- M75: snappy pages ---------------------------------------------------------------------

#[tokio::test]
async fn a_snappy_file_locates_and_reads_back() {
    check_every_row("snappy.parquet", "good.vectors.f32", 300, 3).await;
    let (store, key) = put("multipage-snappy.parquet").await;
    let found = locate(&store, &key, "embedding").await.unwrap();
    assert!(found.extents.len() > 1);
    assert!(found.extents.iter().all(|e| e.compressed > 0));
    check_every_row(
        "multipage-snappy.parquet",
        "multipage.vectors.f32",
        2600,
        found.extents.len(),
    )
    .await;
    // A compressed row has no per-row range.
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert!(col.row_range(5).is_none());
}

#[tokio::test]
async fn a_snappy_page_is_read_once_for_all_its_candidates() {
    use pstore_blob::Accounted;
    let acct = Accounted::new(MemoryStore::new());
    let store = acct.as_tenant(pstore_types::TenantId(1));
    let key = Key::new("lake/ms.parquet");
    store
        .put(&key, bytes("multipage-snappy.parquet").into())
        .await
        .unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let (a, b) = (&col.extents[0], &col.extents[1]);
    let moved = |rows: Vec<usize>| {
        let (acct, store, col, key) = (&acct, &store, &col, &key);
        async move {
            acct.record_ranges();
            let got = col.read_rows(store, &rows).await.unwrap();
            assert_eq!(got.len(), rows.len());
            acct.bytes_in(key, 0..col.len)
        }
    };
    let r0 = a.first_row as usize;
    assert_eq!(moved(vec![r0, r0 + 1]).await, a.compressed, "one page once");
    // Two adjacent pages: both, plus at most the page header the coalescer bridges.
    let two = moved(vec![r0, b.first_row as usize]).await;
    assert!(
        (a.compressed + b.compressed..=a.compressed + b.compressed + 64).contains(&two),
        "{two}"
    );
}

#[tokio::test]
async fn a_damaged_snappy_page_is_an_error() {
    let (store, key) = put("multipage-snappy.parquet").await;
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let e = col.extents[0];
    // Scribble over the compressed body: either it does not decode, or it decodes to the wrong
    // length. Neither may return floats.
    let mut bad = bytes("multipage-snappy.parquet");
    for b in &mut bad[e.offset as usize..(e.offset + 40) as usize] {
        *b = 0xFF;
    }
    store.put(&key, bad.into()).await.unwrap();
    // The object changed, so re-pin to the damaged bytes: the footer still describes them.
    let damaged = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert!(matches!(
        damaged.read_rows(&store, &[e.first_row as usize]).await,
        Err(ExternalError::Damaged)
    ));
    // A page that decompresses cleanly to the wrong length: snappy of 8 bytes where 32 are due.
    let mut short = bytes("multipage-snappy.parquet");
    let tiny = snap::raw::Encoder::new().compress_vec(&[7u8; 8]).unwrap();
    let mut fixed = col.clone();
    fixed.extents[0].compressed = tiny.len() as u64;
    let at = e.offset as usize;
    short[at..at + tiny.len()].copy_from_slice(&tiny);
    store.put(&key, short.into()).await.unwrap();
    fixed.tag = store.get_tag(&key).await.unwrap();
    assert!(matches!(
        fixed.read_rows(&store, &[e.first_row as usize]).await,
        Err(ExternalError::Damaged)
    ));
}

#[tokio::test]
async fn snappy_pages_are_checked_against_their_claims() {
    // Located as compressed: the page's on-disk size is the extent, not `values x width`.
    let mut ok = spec(4, vec![chunk(4, &[5])]);
    ok.chunks[0].codec = 1;
    ok.chunks[0].pages[0].compressed = 30;
    ok.chunks[0].pages[0].body = 30;
    let found = run(&ok).await.unwrap();
    assert_eq!(
        (found.extents[0].rows, found.extents[0].compressed),
        (5, 30)
    );
    // A compressed size of 0 holds nothing.
    let mut empty = ok.clone();
    empty.chunks[0].pages[0].compressed = 0;
    empty.chunks[0].pages[0].body = 0;
    refused(&empty, "levels").await;
    // A page that decompresses to the wrong size carries levels or another layout.
    let mut wrong = ok.clone();
    wrong.chunks[0].pages[0].uncompressed += 4;
    refused(&wrong, "levels").await;
    let mut wrong = ok.clone();
    wrong.chunks[0].pages[0].uncompressed -= 4;
    refused(&wrong, "levels").await;
    // A compressed page longer than its chunk.
    let mut long = ok.clone();
    long.chunks[0].pages[0].body -= 8;
    refused(&long, "past its chunk").await;
}

#[test]
fn a_compressed_extent_is_bounded_by_its_compressed_size() {
    let e = |compressed| Extent {
        first_row: 0,
        rows: 1000,
        offset: 10,
        compressed,
        codec: u8::from(compressed > 0),
        plain_len: 0,
        dict: None,
    };
    // 1,000 rows of 2 floats would be 8,000 raw bytes, but the page is 50 on disk.
    assert!(ExternalColumn::from_extents("k", 2, 60, None, vec![e(50)]).is_ok());
    assert!(ExternalColumn::from_extents("k", 2, 59, None, vec![e(50)]).is_err());
    assert!(
        ExternalColumn::from_extents("k", 2, 60, None, vec![e(0)]).is_err(),
        "raw: 8,000 bytes"
    );
    let col = ExternalColumn::from_extents("k", 2, 60, None, vec![e(50)]).unwrap();
    assert!(col.row_range(3).is_none());
}

// ---- M76: gzip and zstd ---------------------------------------------------------------------

#[tokio::test]
async fn gzip_and_zstd_files_read_back_every_row() {
    for (file, codec) in [
        ("multipage-gzip.parquet", 2u8),
        ("multipage-zstd.parquet", 6),
    ] {
        let (store, key) = put(file).await;
        let found = locate(&store, &key, "embedding").await.unwrap();
        assert!(found.extents.len() > 1, "{file}");
        assert!(
            found
                .extents
                .iter()
                .all(|e| e.codec == codec && e.compressed > 0),
            "{file}"
        );
        check_every_row(file, "multipage.vectors.f32", 2600, found.extents.len()).await;
    }
    check_every_row("zstd.parquet", "good.vectors.f32", 300, 3).await;
    check_every_row("brotli.parquet", "good.vectors.f32", 300, 3).await;
    // M81: `dict.parquet` is dictionary-encoded (100 distinct vectors a row group).
    check_every_row("dict.parquet", "good.vectors.f32", 300, 3).await;
}

#[tokio::test]
async fn lzo_and_hadoop_lz4_are_refused_by_name() {
    for codec in [3, 5] {
        let mut s = spec(4, vec![chunk(4, &[5])]);
        s.chunks[0].codec = codec;
        refused(&s, "only snappy, gzip, brotli, lz4_raw and zstd").await;
    }
}

/// A column of one compressed page whose extent claims `rows` rows, over `page` bytes.
async fn lying(page: Vec<u8>, codec: u8, rows: u64) -> Result<(), ExternalError> {
    let store = MemoryStore::new();
    let key = Key::new("lake/lie.bin");
    store.put(&key, page.clone().into()).await.unwrap();
    let col = ExternalColumn::from_extents(
        "lake/lie.bin",
        2,
        page.len() as u64,
        store.get_tag(&key).await.unwrap(),
        vec![Extent {
            first_row: 0,
            rows,
            offset: 0,
            compressed: page.len() as u64,
            codec,
            plain_len: 0,
            dict: None,
        }],
    )
    .unwrap();
    col.read_rows(&store, &[0]).await.map(|_| ())
}

#[tokio::test]
async fn a_page_that_lies_about_its_size_cannot_balloon() {
    use std::io::Write;
    let zeros = vec![0u8; 64 << 20];
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&zeros).unwrap();
    let gz = gz.finish().unwrap();
    let zs = zstd::stream::encode_all(&zeros[..], 1).unwrap();
    assert!(
        gz.len() < 1 << 20 && zs.len() < 1 << 20,
        "the lies are tiny on disk"
    );
    // The extent claims 4 rows of 2 floats = 32 bytes; the page would inflate to 64 MB.
    assert!(matches!(lying(gz, 2, 4).await, Err(ExternalError::Damaged)));
    assert!(matches!(lying(zs, 6, 4).await, Err(ExternalError::Damaged)));
    // The same pages told the truth are fine.
    let ok_gz = {
        let mut g = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        g.write_all(&[0u8; 32]).unwrap();
        g.finish().unwrap()
    };
    lying(ok_gz, 2, 4).await.unwrap();
    lying(zstd::stream::encode_all(&[0u8; 32][..], 1).unwrap(), 6, 4)
        .await
        .unwrap();
    let br = {
        let mut out = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, 1, 22);
            w.write_all(&zeros).unwrap();
        }
        out
    };
    let lz = lz4_flex::block::compress(&zeros);
    assert!(br.len() < 1 << 20 && lz.len() < 1 << 20);
    assert!(matches!(
        lying(br.clone(), 4, 4).await,
        Err(ExternalError::Damaged)
    ));
    assert!(matches!(lying(lz, 7, 4).await, Err(ExternalError::Damaged)));
    // Told the truth, they read.
    let mut ok_br = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut ok_br, 4096, 1, 22);
        w.write_all(&[0u8; 32]).unwrap();
    }
    lying(ok_br, 4, 4).await.unwrap();
    lying(lz4_flex::block::compress(&[0u8; 32]), 7, 4)
        .await
        .unwrap();
    // A codec number the extent cannot have.
    assert!(matches!(
        lying(vec![1, 2, 3], 9, 4).await,
        Err(ExternalError::Damaged)
    ));
}

#[tokio::test]
async fn a_damaged_page_is_an_error_per_codec() {
    for (file, codec) in [
        ("multipage-gzip.parquet", 2u8),
        ("multipage-zstd.parquet", 6),
    ] {
        let (store, key) = put(file).await;
        let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
            .await
            .unwrap();
        let e = col.extents[0];
        assert_eq!(e.codec, codec);
        let mut bad = bytes(file);
        let at = e.offset as usize;
        for b in &mut bad[at..at + 64] {
            *b = 0xA5;
        }
        store.put(&key, bad.into()).await.unwrap();
        let damaged = ExternalColumn::pin_parquet(&store, &key, "embedding")
            .await
            .unwrap();
        assert!(
            matches!(
                damaged.read_rows(&store, &[e.first_row as usize]).await,
                Err(ExternalError::Damaged)
            ),
            "{file}"
        );
    }
}

// ---- M80: v2 data pages ---------------------------------------------------------------------

#[tokio::test]
async fn v2_files_read_back_every_row() {
    // pyarrow compresses a v2 page only when that shrinks it (`is_compressed` is false
    // otherwise), and random floats barely shrink under snappy: so the snappy file holds raw
    // pages and the zstd file compressed ones. The reader must serve both from one codec.
    for (file, any_compressed) in [
        ("multipage-v2.parquet", false),
        ("multipage-v2-snappy.parquet", false),
        ("multipage-v2-zstd.parquet", true),
    ] {
        let (store, key) = put(file).await;
        let found = locate(&store, &key, "embedding").await.unwrap();
        assert!(found.extents.len() > 1, "{file}");
        // A page is raw (codec 0, no compressed size) or compressed (both set), never half.
        assert!(
            found
                .extents
                .iter()
                .all(|e| (e.codec == 0) == (e.compressed == 0)),
            "{file}: {:?}",
            found.extents
        );
        assert_eq!(
            found.extents.iter().any(|e| e.compressed > 0),
            any_compressed,
            "{file}"
        );
        check_every_row(file, "multipage.vectors.f32", 2600, found.extents.len()).await;
    }
}

#[tokio::test]
async fn a_v2_page_is_checked_against_its_claims() {
    // A v2 page of a REQUIRED flat column: no nulls, no levels, raw or compressed.
    let v2 = |nulls, dl, rl, comp| {
        let mut s = spec(4, vec![chunk(4, &[5])]);
        s.chunks[0].pages[0].kind = 3;
        s.chunks[0].pages[0].v2 = Some((nulls, dl, rl, comp));
        s
    };
    let ok = run(&v2(0, 0, 0, true)).await.unwrap();
    assert_eq!(
        (ok.extents[0].rows, ok.extents[0].codec),
        (5, 0),
        "codec 0 chunk: raw"
    );
    for (what, s) in [
        ("nulls", v2(1, 0, 0, true)),
        ("a definition level", v2(0, 2, 0, true)),
        ("a repetition level", v2(0, 0, 2, true)),
    ] {
        refused(&s, "levels or nulls").await;
        let _ = what;
    }
    // A snappy chunk whose v2 page says it is not compressed reads as raw bytes.
    let mut raw = v2(0, 0, 0, false);
    raw.chunks[0].codec = 1;
    let found = run(&raw).await.unwrap();
    assert_eq!(
        (found.extents[0].codec, found.extents[0].compressed),
        (0, 0)
    );
    // The same chunk with a compressed page reads as snappy, sized by its on-disk bytes.
    let mut comp = v2(0, 0, 0, true);
    comp.chunks[0].codec = 1;
    comp.chunks[0].pages[0].compressed = 30;
    comp.chunks[0].pages[0].body = 30;
    let found = run(&comp).await.unwrap();
    assert_eq!(
        (found.extents[0].codec, found.extents[0].compressed),
        (1, 30)
    );
    // A v2 header on a v1 page type, and a v1 header on a v2 page type, are neither.
    let mut mismatched = v2(0, 0, 0, true);
    mismatched.chunks[0].pages[0].kind = 0;
    refused(&mismatched, "page header").await;
    let mut other = spec(4, vec![chunk(4, &[5])]);
    other.chunks[0].pages[0].kind = 3;
    refused(&other, "page header").await;
}

// ---- M81: dictionary-encoded pages ----------------------------------------------------------

#[tokio::test]
async fn dictionary_files_read_back_every_row() {
    for file in [
        "dictionary.parquet",
        "dictionary-snappy.parquet",
        "dictionary-v2.parquet",
    ] {
        let (store, key) = put(file).await;
        let found = locate(&store, &key, "embedding").await.unwrap();
        assert!(found.extents.len() > 1, "{file}: several index pages");
        assert!(found.extents.iter().all(|e| e.dict.is_some()), "{file}");
        // One dictionary a chunk: every page points at the same one.
        let first = found.extents[0].dict;
        assert!(found.extents.iter().all(|e| e.dict == first), "{file}");
        assert_eq!(first.unwrap().count, 40, "{file}: 40 distinct vectors");
        check_every_row(file, "dictionary.vectors.f32", 2600, found.extents.len()).await;
    }
    // The snappy file's pages (and its dictionary) are compressed.
    let (store, key) = put("dictionary-snappy.parquet").await;
    let found = locate(&store, &key, "embedding").await.unwrap();
    assert!(found.extents.iter().all(|e| e.codec == 1));
    assert_eq!(found.extents[0].dict.unwrap().codec, 1);
}

#[tokio::test]
async fn a_dictionary_is_read_once_for_all_its_candidates() {
    use pstore_blob::Accounted;
    let acct = Accounted::new(MemoryStore::new());
    let store = acct.as_tenant(pstore_types::TenantId(1));
    let key = Key::new("lake/dict.parquet");
    store
        .put(&key, bytes("dictionary.parquet").into())
        .await
        .unwrap();
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let (a, b) = (col.extents[0], col.extents[1]);
    let d = a.dict.unwrap();
    let moved = |rows: Vec<usize>| {
        let (acct, store, col, key) = (&acct, &store, &col, &key);
        async move {
            acct.record_ranges();
            let got = col.read_rows(store, &rows).await.unwrap();
            assert_eq!(got.len(), rows.len());
            acct.bytes_in(key, 0..col.len)
        }
    };
    let r0 = a.first_row as usize;
    // Two rows of one page: that page and the dictionary, each once (plus at most the header the
    // coalescer bridges between the dictionary page and the first index page).
    let one = moved(vec![r0, r0 + 1]).await;
    let want_one = a.compressed + d.compressed;
    assert!(
        (want_one..=want_one + 64).contains(&one),
        "{one} vs {want_one}"
    );
    // Rows on two pages: both pages and still one dictionary (the coalescer may bridge the
    // page header between neighbours).
    let two = moved(vec![r0, b.first_row as usize]).await;
    let want = a.compressed + b.compressed + d.compressed;
    assert!((want..=want + 64).contains(&two), "{two} vs {want}");
}

#[tokio::test]
async fn a_damaged_dictionary_page_is_an_error() {
    let (store, key) = put("dictionary.parquet").await;
    let col = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    let e = col.extents[0];
    // The index page scribbled: a run that is not `rows` indices, or one past the dictionary.
    let mut bad = bytes("dictionary.parquet");
    let at = e.offset as usize;
    for b in &mut bad[at..at + 16] {
        *b = 0xFF;
    }
    store.put(&key, bad.into()).await.unwrap();
    let damaged = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    assert!(matches!(
        damaged.read_rows(&store, &[e.first_row as usize]).await,
        Err(ExternalError::Damaged)
    ));
    // A dictionary that claims more vectors than its bytes hold.
    let (store, key) = put("dictionary.parquet").await;
    let mut lying = ExternalColumn::pin_parquet(&store, &key, "embedding")
        .await
        .unwrap();
    for x in &mut lying.extents {
        x.dict.as_mut().unwrap().count += 1;
    }
    assert!(matches!(
        lying.read_rows(&store, &[0]).await,
        Err(ExternalError::Damaged)
    ));
    // Extents that point a dictionary outside the object, or nowhere, are not a column.
    let ok = col.extents[0];
    let mk = |dict: pstore_format::external::DictRef| {
        ExternalColumn::from_extents(
            "k",
            8,
            col.len,
            None,
            vec![Extent {
                dict: Some(dict),
                ..ok
            }],
        )
    };
    let d = ok.dict.unwrap();
    assert!(mk(d).is_ok());
    // A dictionary that ends exactly at the object's end is inside it; one byte more is not.
    let last = pstore_format::external::DictRef {
        offset: col.len - d.compressed,
        ..d
    };
    assert!(mk(last).is_ok());
    assert!(
        mk(pstore_format::external::DictRef {
            offset: last.offset + 1,
            ..last
        })
        .is_err()
    );
    assert!(mk(pstore_format::external::DictRef { count: 0, ..d }).is_err());
    assert!(
        mk(pstore_format::external::DictRef {
            offset: col.len,
            ..d
        })
        .is_err()
    );
    assert!(
        mk(pstore_format::external::DictRef {
            offset: u64::MAX,
            ..d
        })
        .is_err(),
        "overflow"
    );
    let no_body = Extent {
        compressed: 0,
        dict: Some(d),
        ..ok
    };
    assert!(ExternalColumn::from_extents("k", 8, col.len, None, vec![no_body]).is_err());
}

/// A chunk with a dictionary of `count` four-wide vectors, then index pages of `rows` rows each
/// (6 bytes of indices apiece).
fn dict_spec(count: i64, rows: &[i64]) -> Spec {
    let bytes = count * 16;
    let mut pages = vec![Page {
        values: count,
        compressed: bytes,
        uncompressed: bytes,
        encoding: 0,
        kind: 2,
        body: bytes as usize,
        v2: None,
    }];
    for r in rows {
        pages.push(Page {
            values: *r,
            compressed: 6,
            uncompressed: 6,
            encoding: 8,
            kind: 0,
            body: 6,
            v2: None,
        });
    }
    let n = rows.iter().sum();
    spec(
        4,
        vec![Chunk {
            pages,
            num_values: n,
            total_compressed: None,
            codec: 0,
            encodings: vec![0, 3, 8],
            dictionary_offset: true,
            dictionary_zero: false,
            chunk_type: 7,
            group_rows: n,
        }],
    )
}

#[tokio::test]
async fn a_dictionary_chunk_is_checked_page_by_page() {
    let ok = run(&dict_spec(5, &[3, 4])).await.unwrap();
    assert_eq!(ok.extents.len(), 2);
    let d = ok.extents[0].dict.unwrap();
    assert_eq!((d.count, d.codec, d.compressed), (5, 0, 80));
    assert!(
        ok.extents
            .iter()
            .all(|e| e.dict == Some(d) && e.plain_len == 6)
    );
    // PLAIN_DICTIONARY (2) names the dictionary page's encoding as well as PLAIN (0).
    let mut s = dict_spec(5, &[3]);
    s.chunks[0].pages[0].encoding = 2;
    run(&s).await.unwrap();
    s.chunks[0].pages[0].encoding = 5;
    refused(&s, "dictionary page is not PLAIN").await;
    // An empty dictionary, and one whose size is not count x width.
    refused(&dict_spec(0, &[3]), "dictionary size").await;
    let mut s = dict_spec(5, &[3]);
    s.chunks[0].pages[0].uncompressed = 79;
    refused(&s, "not a plain run").await;
    s.chunks[0].pages[0].uncompressed = 80;
    s.chunks[0].pages[0].compressed = 79;
    refused(&s, "not a plain run").await;
    // Compressed: the on-disk size is free but must exist.
    let mut s = dict_spec(5, &[3]);
    s.chunks[0].codec = 1;
    s.chunks[0].pages[0].compressed = 20;
    s.chunks[0].pages[0].body = 20;
    let found = run(&s).await.unwrap();
    assert_eq!(found.extents[0].dict.unwrap().compressed, 20);
    s.chunks[0].pages[0].compressed = 0;
    s.chunks[0].pages[0].body = 0;
    refused(&s, "not a plain run").await;
    // A dictionary bigger than any page may decode to is refused before it is read: 2^24 vectors
    // of 16 bytes is exactly 256 MiB (allowed, so it is only the chunk that is too short), one
    // more is not.
    let mut s = dict_spec(1 << 24, &[3]);
    s.chunks[0].codec = 1;
    s.chunks[0].pages[0].compressed = 20;
    s.chunks[0].pages[0].body = 20;
    run(&s).await.unwrap();
    let mut s = dict_spec((1 << 24) + 1, &[3]);
    s.chunks[0].codec = 1;
    s.chunks[0].pages[0].compressed = 20;
    s.chunks[0].pages[0].body = 20;
    refused(&s, "not a plain run").await;
    // A dictionary that runs past its chunk, or fills it with no index page after.
    let mut s = dict_spec(5, &[3]);
    s.chunks[0].total_compressed = Some(70);
    refused(&s, "runs past its chunk").await;
    let mut s = dict_spec(5, &[]);
    s.chunks[0].num_values = 3;
    s.chunks[0].group_rows = 3;
    refused(&s, "pages end before").await;
    // Two dictionaries, and one that does not come first.
    let mut s = dict_spec(5, &[3]);
    let again = s.chunks[0].pages[0].clone();
    s.chunks[0].pages.insert(1, again);
    refused(&s, "dictionary page out of place").await;
}

#[tokio::test]
async fn an_index_page_is_checked_against_its_sizes() {
    let with = |f: &dyn Fn(&mut Page)| {
        let mut s = dict_spec(5, &[3]);
        f(&mut s.chunks[0].pages[1]);
        s
    };
    run(&with(&|_| {})).await.unwrap();
    refused(&with(&|p| p.uncompressed = 0), "impossible size").await;
    refused(&with(&|p| p.compressed = 0), "impossible size").await;
    // Raw, the body is its own size; PLAIN_DICTIONARY (2) is an index page too.
    refused(&with(&|p| p.compressed = 5), "impossible size").await;
    run(&with(&|p| p.encoding = 2)).await.unwrap();
    // Compressed, it may shrink; and it may decode to 256 MiB but not a byte more.
    let squeezed = |un: i64| {
        let mut s = dict_spec(5, &[3]);
        s.chunks[0].codec = 1;
        s.chunks[0].pages[0].compressed = 20;
        s.chunks[0].pages[0].body = 20;
        s.chunks[0].pages[1].uncompressed = un;
        s
    };
    assert_eq!(run(&squeezed(900)).await.unwrap().extents[0].plain_len, 900);
    run(&squeezed(256 << 20)).await.unwrap();
    refused(&squeezed((256 << 20) + 1), "impossible size").await;
    // An index page of no rows, and a body that runs past the chunk.
    refused(&dict_spec(5, &[0, 3]), "runs past its chunk").await;
    let mut s = dict_spec(5, &[3]);
    s.chunks[0].total_compressed = Some(85);
    refused(&s, "runs past its chunk").await;
    // More rows than the chunk holds.
    let mut s = dict_spec(5, &[3, 4]);
    s.chunks[0].num_values = 5;
    s.chunks[0].group_rows = 5;
    refused(&s, "overshoot").await;
}

// ---- M84: OPTIONAL columns that hold no nulls -------------------------------------------------

#[tokio::test]
async fn optional_files_without_nulls_read_back_every_row() {
    for (file, codec) in [
        ("optional.parquet", 0u8),
        ("optional-multipage.parquet", 0),
        ("optional-v2.parquet", 0),
        ("optional-v2-zstd.parquet", 6),
    ] {
        let (store, key) = put(file).await;
        let found = locate(&store, &key, "embedding").await.unwrap();
        // The levels are stepped over: no extent starts where its page's body does.
        assert!(
            found
                .extents
                .iter()
                .all(|e| e.codec == 0 || e.codec == codec),
            "{file}"
        );
        if file == "optional.parquet" {
            check_every_row(file, "good.vectors.f32", 300, 3).await;
        } else {
            check_every_row(file, "multipage.vectors.f32", 2600, found.extents.len()).await;
        }
    }
}

#[tokio::test]
async fn an_optional_column_that_cannot_be_read_in_place_is_refused_by_name() {
    for (file, why) in [
        ("optional-nulls.parquet", "holds nulls"),
        (
            "optional-snappy.parquet",
            "levels are inside the compressed body",
        ),
        ("optional-dictionary.parquet", "dictionary-encoded optional"),
    ] {
        let (store, key) = put(file).await;
        let err = locate(&store, &key, "embedding").await.unwrap_err();
        assert!(err.to_string().contains(why), "{file}: {err}");
    }
}

#[tokio::test]
async fn damaged_definition_levels_are_refused() {
    let (store, key) = put("optional-multipage.parquet").await;
    let found = locate(&store, &key, "embedding").await.unwrap();
    let first = found.extents[0];
    let raw = bytes("optional-multipage.parquet");
    // v1: [length: u32][levels][values]. The levels end where the values begin.
    let at = first.offset as usize;
    let rle_value = at - 1;
    assert_eq!(raw[rle_value], 1, "a run of 'present'");
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = raw.clone();
        f(&mut b);
        b
    };
    let len_at = {
        // The 4-byte length sits `4 + L` bytes before the values; L = 3 for one short run.
        let l = u32::from_le_bytes(raw[at - 4 - 3..at - 3].try_into().unwrap());
        assert_eq!(l, 3, "one run: header (2 bytes) and its value");
        at - 7
    };
    // The levels may run exactly to the end of the page (leaving no values: a different
    // refusal), but not one byte past it.
    let page = 4 + 3 + first.rows as usize * 32;
    let to_end = with(&|b| {
        b[len_at..len_at + 4].copy_from_slice(&((page - 4) as u32).to_le_bytes());
    });
    store.put(&key, to_end.into()).await.unwrap();
    let err = locate(&store, &key, "embedding")
        .await
        .unwrap_err()
        .to_string();
    assert!(!err.contains("run past their page"), "{err}");
    let beyond = with(&|b| {
        b[len_at..len_at + 4].copy_from_slice(&((page - 3) as u32).to_le_bytes());
    });
    store.put(&key, beyond.into()).await.unwrap();
    let err = locate(&store, &key, "embedding")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("run past their page"), "{err}");
    for (what, damaged, why) in [
        ("a null", with(&|b| b[rle_value] = 0), "holds nulls"),
        (
            "a longer level run than the page",
            with(&|b| b[len_at + 3] = 0xFF),
            "run past their page",
        ),
        ("no levels", with(&|b| b[len_at] = 0), "run past their page"),
    ] {
        store.put(&key, damaged.into()).await.unwrap();
        let err = locate(&store, &key, "embedding").await.unwrap_err();
        assert!(err.to_string().contains(why), "{what}: {err}");
    }
}

#[tokio::test]
async fn an_optional_page_is_bounded_before_its_levels_are_read() {
    let optional = |values: i64| {
        let mut s = spec(4, vec![chunk(4, &[3])]);
        s.repetition = 1;
        s.chunks[0].pages[0].values = values;
        s.chunks[0].num_values = values;
        s.chunks[0].group_rows = values;
        s
    };
    // A page of no values runs nowhere (the chunk holds values in a later page).
    let mut empty = spec(4, vec![chunk(4, &[0, 3])]);
    empty.repetition = 1;
    refused(&empty, "runs past its chunk").await;
    // Values that decode to more than 256 MiB are refused before any level is read; exactly
    // 256 MiB (2^24 four-wide vectors) is not, and goes on to its levels.
    refused(&optional((1 << 24) + 1), "too many values").await;
    refused(&optional(1 << 24), "definition levels").await;
}
