//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M82: a float32 vector column of a Lance dataset, read in place.
//!
//! Real pylance 13 datasets (written by `data/generate_lance.py`) cover the shapes a writer
//! produces; a small protobuf builder covers what pylance will not write at this size (several
//! pages) and each way a page table can lie.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::external::{
    AttrKind, AttrSource, AttrValue, CODEC_FASTLANES, ExternalAttribute, ExternalColumn,
    ExternalError,
};
use pstore_format::lance::{LanceError, locate};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/lance/");

/// Every file of the dataset directory `name`, stored under `lake/<name>/...`.
async fn dataset(name: &str) -> (MemoryStore, Key) {
    dataset_in(DIR, name).await
}

/// As [`dataset`], from the directory `dir`.
async fn dataset_in(dir: &str, name: &str) -> (MemoryStore, Key) {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let root = std::path::PathBuf::from(format!("{dir}{name}"));
    let mut files = Vec::new();
    walk(&root, &mut files);
    let store = MemoryStore::new();
    let mut manifest = None;
    for f in files {
        let rel = f.strip_prefix(&root).unwrap().to_str().unwrap().to_owned();
        let key = Key::new(format!("lake/{name}/{rel}"));
        store
            .put(&key, std::fs::read(&f).unwrap().into())
            .await
            .unwrap();
        // The newest manifest is the one with the smallest number (V2 naming counts down).
        if rel.ends_with(".manifest")
            && manifest
                .as_ref()
                .is_none_or(|(r, _): &(String, Key)| rel < *r)
        {
            manifest = Some((rel, key));
        }
    }
    (store, manifest.unwrap().1)
}

fn floats() -> Vec<f32> {
    std::fs::read(format!("{DIR}vectors.f32"))
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

async fn refused(name: &str, why: &str) {
    let (store, manifest) = dataset(name).await;
    let err = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains(why),
        "{name}: wanted {why:?}, got {err}"
    );
}

#[tokio::test]
async fn a_lance_dataset_reads_back_every_row() {
    let (store, manifest) = dataset("one").await;
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    assert_eq!((col.dims, col.rows), (8, 2600));
    assert!(
        col.key.contains("/data/"),
        "bound to the data file: {}",
        col.key
    );
    let want = floats();
    let all: Vec<usize> = (0..2600).collect();
    let got = col.read_rows(&store, &all).await.unwrap();
    assert_eq!(got.len(), 2600);
    for (r, v) in &got {
        assert_eq!(v.as_slice(), &want[r * 8..r * 8 + 8], "row {r}");
    }
    // Nothing but the column's own bytes is read for one row.
    assert_eq!(col.row_range(7), Some(col.row_range(7).unwrap()));
    assert_eq!(
        col.row_range(7).unwrap().end - col.row_range(7).unwrap().start,
        32
    );
}

#[tokio::test]
async fn a_column_that_cannot_be_read_in_place_is_refused_by_name() {
    refused("nullable", "null-free float32").await;
    // A column the dataset does not have, and one that is not a vector.
    let (store, manifest) = dataset("one").await;
    for (col, why) in [
        ("nope", "no top-level column"),
        ("id", "not a float32 fixed-size list"),
    ] {
        let err = locate(&store, &manifest, col).await.unwrap_err();
        assert!(err.to_string().contains(why), "{col}: {err}");
    }
}

// ---- a protobuf builder, for several pages and each way a page table can lie ----------------

fn varint(mut v: u64) -> Vec<u8> {
    let mut b = Vec::new();
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            b.push(byte);
            return b;
        }
        b.push(byte | 0x80);
    }
}
fn num(field: u64, v: u64) -> Vec<u8> {
    [varint(field << 3), varint(v)].concat()
}
fn msg(field: u64, body: &[u8]) -> Vec<u8> {
    [
        varint(field << 3 | 2),
        varint(body.len() as u64),
        body.to_vec(),
    ]
    .concat()
}
fn packed(field: u64, vs: &[u64]) -> Vec<u8> {
    msg(
        field,
        &vs.iter().flat_map(|v| varint(*v)).collect::<Vec<_>>(),
    )
}

/// What a synthetic dataset claims; each test bends one field.
#[derive(Clone)]
struct Spec {
    dims: u64,
    /// `(rows, page bytes override)` for each page.
    pages: Vec<(u64, Option<u64>)>,
    logical: String,
    bits: u64,
    page_dims: u64,
    buffers: usize,
    url: String,
    physical_rows: Option<u64>,
    file_size: Option<u64>,
    manifest_path: &'static str,
    data_path: String,
    file_version: (u64, u64),
    footer_version: (u16, u16),
    page_offset_shift: u64,
    /// The column-metadata size the table claims, instead of the true one.
    meta_claim: Option<u64>,
    /// The metadata the table claims runs exactly to the end of the file.
    meta_to_end: bool,
    /// Zeros appended after the metadata, before the table that points at it.
    pad: usize,
    /// The last page ends exactly at the end of the file (offset computed to fit).
    last_page_to_end: bool,
    buffer_index: u64,
    buffer_type: u64,
    /// Used by `build`: the file length the last page must end at.
    last_page_end_guess: u64,
    /// Fixed-width fields a reader must skip, in the manifest and every page.
    noise: bool,
    magic: &'static [u8; 4],
}

fn spec() -> Spec {
    Spec {
        dims: 2,
        pages: vec![(3, None), (4, None), (5, None)],
        logical: "fixed_size_list:float:2".to_owned(),
        bits: 32,
        page_dims: 2,
        buffers: 1,
        url: "/lance.encodings.ArrayEncoding".to_owned(),
        physical_rows: None,
        file_size: None,
        manifest_path: "ds/_versions/1.manifest",
        data_path: "a.lance".to_owned(),
        file_version: (2, 0),
        footer_version: (0, 3),
        page_offset_shift: 0,
        meta_claim: None,
        meta_to_end: false,
        pad: 0,
        last_page_to_end: false,
        buffer_index: 0,
        buffer_type: 0,
        last_page_end_guess: 0,
        noise: false,
        magic: b"LANC",
    }
}

/// A fixed64 and a fixed32 field, which the reader must step over.
fn noise(s: &Spec) -> Vec<u8> {
    if !s.noise {
        return Vec::new();
    }
    [
        varint(15 << 3 | 1),
        7u64.to_le_bytes().to_vec(),
        varint(14 << 3 | 5),
        7u32.to_le_bytes().to_vec(),
    ]
    .concat()
}

/// `(store, manifest key)` for `s`: a data file of its pages' floats (the row number as `f32`
/// in every component), and a manifest naming it.
async fn build(s: &Spec) -> (MemoryStore, Key) {
    if s.last_page_to_end {
        // The offsets' own width changes the file's length: iterate to a fixed point.
        let mut guess = 0u64;
        for _ in 0..4 {
            let mut probe = s.clone();
            probe.last_page_to_end = false;
            probe.last_page_end_guess = guess;
            let (store, key) = build_once(&probe).await;
            let dkey = Key::new(format!("lake/ds/data/{}", s.data_path));
            let len = store.head(&dkey).await.unwrap();
            if guess == len {
                return (store, key);
            }
            guess = len;
        }
        panic!("no fixed point");
    }
    build_once(s).await
}

async fn build_once(s: &Spec) -> (MemoryStore, Key) {
    let mut data = Vec::new();
    let mut pages = Vec::new();
    let mut row = 0u32;
    for (rows, size) in &s.pages {
        let at = data.len() as u64;
        for _ in 0..*rows {
            for _ in 0..s.dims {
                data.extend((row as f32).to_le_bytes());
            }
            row += 1;
        }
        let len = size.unwrap_or(rows * s.dims * 4);
        let buffer = msg(2, &[num(1, s.buffer_index), num(2, s.buffer_type)].concat());
        let flat = msg(1, &[num(1, s.bits), buffer].concat());
        let values = msg(1, &flat);
        let no_null = msg(1, &values);
        let items = msg(2, &no_null);
        let fsl = msg(3, &[num(1, s.page_dims), msg(2, &items)].concat());
        let column = msg(2, &msg(1, &msg(1, &fsl)));
        let any = [msg(1, s.url.as_bytes()), msg(2, &column)].concat();
        let encoding = msg(4, &msg(2, &msg(1, &any)));
        let last = s.last_page_end_guess != 0 && rows == &s.pages.last().unwrap().0 && at > 0;
        let at_here = if last {
            s.last_page_end_guess - len
        } else {
            at
        };
        let offsets: Vec<u64> = (0..s.buffers)
            .map(|_| at_here + s.page_offset_shift)
            .collect();
        let sizes: Vec<u64> = (0..s.buffers).map(|_| len).collect();
        pages.push(msg(
            2,
            &[
                packed(1, &offsets),
                packed(2, &sizes),
                num(3, *rows),
                encoding,
                noise(s),
            ]
            .concat(),
        ));
    }
    let meta = pages.concat();
    let meta_at = data.len() as u64;
    data.extend(&meta);
    data.extend(vec![0u8; s.pad]);
    let table_at = data.len() as u64;
    data.extend(meta_at.to_le_bytes());
    // After the claim come the table (16 bytes) and the footer (40).
    let to_end = (meta.len() + s.pad + 16 + 40) as u64;
    let claim = if s.meta_to_end {
        Some(to_end)
    } else {
        s.meta_claim
    };
    data.extend(claim.unwrap_or(meta.len() as u64).to_le_bytes());
    let gbo_at = data.len() as u64;
    for v in [meta_at, table_at, gbo_at] {
        data.extend(v.to_le_bytes());
    }
    data.extend(0u32.to_le_bytes());
    data.extend(1u32.to_le_bytes());
    data.extend(s.footer_version.0.to_le_bytes());
    data.extend(s.footer_version.1.to_le_bytes());
    data.extend(s.magic);

    let total_rows: u64 = s.pages.iter().map(|p| p.0).sum();
    let field = msg(
        1,
        &[
            msg(2, b"emb"),
            num(3, 0),
            num(4, u64::MAX),
            msg(5, s.logical.as_bytes()),
        ]
        .concat(),
    );
    let file = msg(
        2,
        &[
            msg(1, s.data_path.as_bytes()),
            packed(2, &[0]),
            packed(3, &[0]),
            num(4, s.file_version.0),
            num(5, s.file_version.1),
            num(6, s.file_size.unwrap_or(data.len() as u64)),
        ]
        .concat(),
    );
    let fragment = msg(
        2,
        &[file, num(4, s.physical_rows.unwrap_or(total_rows))].concat(),
    );
    let body = [field, fragment, noise(s)].concat();
    let mut manifest = (body.len() as u32).to_le_bytes().to_vec();
    manifest.extend(&body);
    manifest.extend(0u64.to_le_bytes());
    manifest.extend(0u16.to_le_bytes());
    manifest.extend(2u16.to_le_bytes());
    manifest.extend(b"LANC");

    let store = MemoryStore::new();
    let key = Key::new(format!("lake/{}", s.manifest_path));
    store.put(&key, manifest.into()).await.unwrap();
    let dkey = Key::new(format!("lake/ds/data/{}", s.data_path));
    store.put(&dkey, data.into()).await.unwrap();
    (store, key)
}

async fn err_of(s: &Spec) -> LanceError {
    let (store, key) = build(s).await;
    locate(&store, &key, "emb").await.unwrap_err()
}

async fn rejected(s: &Spec, why: &str) {
    let e = err_of(s).await.to_string();
    assert!(e.contains(why), "wanted {why:?}, got {e}");
}

#[tokio::test]
async fn several_pages_are_one_extent_each() {
    let (store, key) = build(&spec()).await;
    let col = ExternalColumn::pin_lance(&store, &key, "emb")
        .await
        .unwrap();
    assert_eq!((col.dims, col.rows, col.extents.len()), (2, 12, 3));
    let rows: Vec<(u64, u64)> = col.extents.iter().map(|e| (e.first_row, e.rows)).collect();
    assert_eq!(rows, [(0, 3), (3, 4), (7, 5)]);
    let got = col
        .read_rows(&store, &(0..12).collect::<Vec<_>>())
        .await
        .unwrap();
    for (r, v) in got {
        assert_eq!(v, [r as f32, r as f32]);
    }
}

#[tokio::test]
async fn a_page_table_that_lies_is_refused() {
    let with = |f: &dyn Fn(&mut Spec)| {
        let mut s = spec();
        f(&mut s);
        s
    };
    rejected(&with(&|s| s.pages[1].1 = Some(31)), "rows x dims x 4").await;
    rejected(&with(&|s| s.pages[1].1 = Some(33)), "rows x dims x 4").await;
    rejected(&with(&|s| s.pages[0].0 = 0), "rows x dims x 4").await;
    rejected(&with(&|s| s.page_offset_shift = 1 << 20), "past the file").await;
    rejected(&with(&|s| s.buffers = 2), "more than one buffer").await;
    rejected(&with(&|s| s.buffers = 0), "more than one buffer").await;
    rejected(&with(&|s| s.bits = 64), "null-free float32").await;
    rejected(&with(&|s| s.buffer_index = 1), "null-free float32").await;
    rejected(&with(&|s| s.buffer_type = 1), "null-free float32").await;
    rejected(&with(&|s| s.page_dims = 3), "dimension disagrees").await;
    rejected(
        &with(&|s| s.url = "/lance.encodings.Other".to_owned()),
        "null-free float32",
    )
    .await;
    rejected(
        &with(&|s| s.physical_rows = Some(13)),
        "do not hold the fragment's rows",
    )
    .await;
    rejected(
        &with(&|s| s.physical_rows = Some(11)),
        "do not hold the fragment's rows",
    )
    .await;
    rejected(
        &with(&|s| s.pages.clear()),
        "do not hold the fragment's rows",
    )
    .await;
}

#[tokio::test]
async fn the_edges_of_what_the_file_may_hold_are_exact() {
    let (store, key) = build(&Spec {
        noise: true,
        ..spec()
    })
    .await;
    locate(&store, &key, "emb")
        .await
        .expect("fixed-width fields are stepped over");
    // The last page may end exactly at the end of the file (here, with nothing after it that
    // the reader looks at), and one byte past is refused.
    let at_end = Spec {
        last_page_to_end: true,
        ..spec()
    };
    let (store, key) = build(&at_end).await;
    locate(&store, &key, "emb")
        .await
        .expect("a page ending at the file's end is inside it");
    rejected(
        &Spec {
            page_offset_shift: 1,
            ..at_end
        },
        "past the file",
    )
    .await;
    // The column metadata the table claims: exactly the cap is read (and is not protobuf), one
    // more is refused outright; a claim past the file is refused before any read.
    let cap = 8 << 20;
    let padded = |claim| Spec {
        pad: cap + 64,
        meta_claim: Some(claim),
        ..spec()
    };
    let e = err_of(&padded(cap as u64)).await.to_string();
    assert!(!e.contains("runs past the file"), "{e}");
    rejected(
        &padded(cap as u64 + 1),
        "column metadata runs past the file",
    )
    .await;
    rejected(
        &Spec {
            meta_claim: Some(1 << 30),
            ..spec()
        },
        "column metadata runs past the file",
    )
    .await;
    // Claiming exactly to the end of the file is inside it (the bytes are not protobuf, which
    // is a different refusal); one byte more is past it.
    let e = err_of(&Spec {
        meta_to_end: true,
        ..spec()
    })
    .await
    .to_string();
    assert!(!e.contains("runs past the file"), "{e}");
}

#[tokio::test]
async fn a_manifest_or_file_that_is_not_lance_is_refused() {
    let with = |f: &dyn Fn(&mut Spec)| {
        let mut s = spec();
        f(&mut s);
        s
    };
    rejected(
        &with(&|s| s.magic = b"LANX"),
        "no LANC magic on the data file",
    )
    .await;
    rejected(&with(&|s| s.footer_version = (0, 2)), "own version").await;
    rejected(&with(&|s| s.footer_version = (1, 3)), "own version").await;
    // 2.1 is read (M90), but this builder writes a 2.0 footer under it.
    rejected(&with(&|s| s.file_version = (2, 1)), "own version").await;
    rejected(&with(&|s| s.file_version = (0, 3)), "format 0.3").await;
    rejected(&with(&|s| s.file_version = (3, 0)), "format 3.0").await;
    rejected(
        &with(&|s| s.file_size = Some(1)),
        "not the size the manifest says",
    )
    .await;
    rejected(
        &with(&|s| s.manifest_path = "ds/manifest"),
        "under _versions",
    )
    .await;
    rejected(
        &with(&|s| s.data_path = "../x.lance".to_owned()),
        "data file path",
    )
    .await;
    rejected(
        &with(&|s| s.data_path = "/abs.lance".to_owned()),
        "data file path",
    )
    .await;
    rejected(
        &with(&|s| s.logical = "fixed_size_list:double:2".to_owned()),
        "not a float32",
    )
    .await;
    rejected(
        &with(&|s| s.logical = "fixed_size_list:float:0".to_owned()),
        "not a float32",
    )
    .await;
    rejected(&with(&|s| s.logical = "float".to_owned()), "not a float32").await;
    // A manifest cut short, and an object that is not a manifest at all.
    let (store, key) = build(&spec()).await;
    let raw = store
        .get_range(&key, 0..store.head(&key).await.unwrap())
        .await
        .unwrap();
    for (what, bytes) in [
        ("empty", Vec::new()),
        ("tiny", vec![1, 2, 3]),
        ("no magic", raw[..raw.len() - 1].to_vec()),
        ("cut", [&raw[..10], &raw[raw.len() - 16..]].concat()),
    ] {
        store.put(&key, bytes.into()).await.unwrap();
        assert!(locate(&store, &key, "emb").await.is_err(), "{what}");
    }
    // A data file that is gone changes nothing the manifest can say: the read fails.
    let (full, key) = build(&spec()).await;
    let lonely = MemoryStore::new();
    let manifest = full
        .get_range(&key, 0..full.head(&key).await.unwrap())
        .await
        .unwrap();
    lonely.put(&key, manifest).await.unwrap();
    assert!(locate(&lonely, &key, "emb").await.is_err());
}

#[tokio::test]
async fn a_changed_data_file_is_noticed() {
    let (store, key) = build(&spec()).await;
    let col = ExternalColumn::pin_lance(&store, &key, "emb")
        .await
        .unwrap();
    let dkey = Key::new("lake/ds/data/a.lance");
    let mut raw = store
        .get_range(&dkey, 0..store.head(&dkey).await.unwrap())
        .await
        .unwrap()
        .to_vec();
    raw[0] ^= 0xFF;
    store.put(&dkey, raw.into()).await.unwrap();
    assert!(matches!(
        col.read_rows(&store, &[0]).await,
        Err(ExternalError::Changed)
    ));
}

// ---- M87: several data files, and deleted rows ------------------------------------------------

#[tokio::test]
async fn a_multi_fragment_dataset_reads_back_every_row() {
    let (store, manifest) = dataset("many").await;
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    assert_eq!(
        (col.dims, col.rows, col.more.len()),
        (8, 2600, 2),
        "three data files"
    );
    // The runs name all three objects, in row order.
    let objects: Vec<u32> = col.extents.iter().map(|e| e.object).collect();
    assert_eq!(objects.first(), Some(&0));
    assert!(objects.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(objects.last(), Some(&2));
    let want = floats();
    let all: Vec<usize> = (0..2600).collect();
    let got = col.read_rows(&store, &all).await.unwrap();
    assert_eq!(got.len(), 2600);
    for (r, v) in &got {
        assert_eq!(v.as_slice(), &want[r * 8..r * 8 + 8], "row {r}");
    }
    // A read across two data files asks each once, and nothing else.
    let rows = [10usize, 1_500, 2_599];
    let some = col.read_rows(&store, &rows).await.unwrap();
    assert_eq!(some.len(), 3);
}

#[tokio::test]
async fn a_deleted_row_is_never_returned() {
    let (store, manifest) = dataset("deleted").await;
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    // Ids 0..9 were deleted from 300 rows: they keep their numbers and have no vector.
    assert_eq!(col.rows, 300);
    for r in 0..10 {
        assert!(!col.is_present(r), "row {r} is deleted");
        assert!(matches!(
            col.read_rows(&store, &[r, 100]).await,
            Err(ExternalError::Absent(x)) if x == r
        ));
    }
    let want = floats();
    let present: Vec<usize> = (10..300).collect();
    let got = col.read_rows(&store, &present).await.unwrap();
    assert_eq!(got.len(), 290);
    for (r, v) in &got {
        assert_eq!(v.as_slice(), &want[r * 8..r * 8 + 8], "row {r}");
    }
}

#[tokio::test]
async fn a_changed_fragment_is_noticed() {
    let (store, manifest) = dataset("many").await;
    let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap();
    // Replace the last data file; a read that touches only the first still notices.
    let last = Key::new(col.more.last().unwrap().key.clone());
    let mut raw = store
        .get_range(&last, 0..store.head(&last).await.unwrap())
        .await
        .unwrap()
        .to_vec();
    raw[0] ^= 0xFF;
    store.put(&last, raw.into()).await.unwrap();
    assert!(matches!(
        col.read_rows(&store, &[0]).await,
        Err(ExternalError::Changed)
    ));
}

#[tokio::test]
async fn a_deletion_file_that_lies_is_refused() {
    let (store, manifest) = dataset("deleted").await;
    let dir = format!("{DIR}deleted/_deletions");
    let name = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let key = Key::new(format!(
        "lake/deleted/_deletions/{}",
        name.to_str().unwrap()
    ));
    let good = std::fs::read(format!("{dir}/{}", name.to_str().unwrap())).unwrap();
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = good.clone();
        f(&mut b);
        b
    };
    for (what, bad) in [
        ("truncated", good[..good.len() / 2].to_vec()),
        ("empty", Vec::new()),
        ("not arrow", with(&|b| b[0] = b'X')),
        (
            "no trailing magic",
            with(&|b| *b.last_mut().unwrap() = b'X'),
        ),
        (
            "a footer length past the file",
            with(&|b| {
                let n = b.len();
                b[n - 10..n - 6].copy_from_slice(&u32::MAX.to_le_bytes());
            }),
        ),
    ] {
        store.put(&key, bad.into()).await.unwrap();
        let err = locate(&store, &manifest, "emb")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("deletion file"), "{what}: {err}");
    }
    // The real one still works, put back.
    store.put(&key, good.into()).await.unwrap();
    locate(&store, &manifest, "emb").await.unwrap();
}

#[tokio::test]
async fn a_deletion_count_that_disagrees_or_a_file_too_large_is_refused() {
    let (store, manifest) = dataset("deleted").await;
    let raw = store
        .get_range(&manifest, 0..store.head(&manifest).await.unwrap())
        .await
        .unwrap()
        .to_vec();
    // The manifest says "10 rows deleted" (field 4 of the deletion file, then the fragment's
    // 300 physical rows); the file lists 10.
    let pattern = [0x20u8, 0x0A, 0x20, 0xAC, 0x02];
    // The last occurrence: the manifest proper follows an earlier section of the file.
    let at = raw
        .windows(pattern.len())
        .rposition(|w| w == pattern)
        .expect("the deletion count");
    let with_count = |n: u8| {
        let mut b = raw.clone();
        b[at + 1] = n;
        b
    };
    for (count, ok) in [(10u8, true), (0, true), (9, false), (11, false)] {
        store
            .put(&manifest, with_count(count).into())
            .await
            .unwrap();
        let got = locate(&store, &manifest, "emb").await;
        assert_eq!(got.is_ok(), ok, "count {count}: {:?}", got.err());
        if !ok {
            assert!(got.unwrap_err().to_string().contains("different number"));
        }
    }
    // A deletion file past 64 MiB is refused unread; exactly 64 MiB is read, and is not Arrow.
    store.put(&manifest, raw.into()).await.unwrap();
    let dir = format!("{DIR}deleted/_deletions");
    let name = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let key = Key::new(format!(
        "lake/deleted/_deletions/{}",
        name.to_str().unwrap()
    ));
    store
        .put(&key, vec![0u8; (64 << 20) + 1].into())
        .await
        .unwrap();
    let err = locate(&store, &manifest, "emb")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("too large"), "{err}");
    store.put(&key, vec![0u8; 64 << 20].into()).await.unwrap();
    let err = locate(&store, &manifest, "emb")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("deletion file") && !err.contains("too large"),
        "{err}"
    );
}

// ---- M89: the version a dataset's hint names -------------------------------------------------

#[tokio::test]
async fn the_hint_names_the_newest_version() {
    // `deleted` has two versions (the second deletes ten rows); `many` has one.
    let (store, newest) = dataset("deleted").await;
    let got = pstore_format::lance::latest_manifest(&store, "lake/deleted")
        .await
        .unwrap();
    assert_eq!(got, newest);
    assert!(got.as_str().ends_with("18446744073709551613.manifest"));
    let col = ExternalColumn::pin_lance_latest(&store, "lake/deleted", "emb")
        .await
        .unwrap();
    assert!(!col.is_present(0), "the newest version has the deletions");
    let (store, one) = dataset("many").await;
    assert_eq!(
        pstore_format::lance::latest_manifest(&store, "lake/many/")
            .await
            .unwrap(),
        one,
        "a trailing slash is fine"
    );
}

#[tokio::test]
async fn a_hint_that_lies_is_refused_never_guessed_around() {
    let (store, _) = dataset("deleted").await;
    let hint = Key::new("lake/deleted/_versions/latest_version_hint.json");
    for (what, body, why) in [
        ("not json", "version 2", "not {"),
        ("no version", "{}", "not {"),
        ("zero", "{\"version\":0}", "not {"),
        ("negative", "{\"version\":-1}", "not {"),
        ("a float", "{\"version\":2.0}", "not {"),
        ("a string", "{\"version\":\"2\"}", "not {"),
        ("extra keys", "{\"version\":2,\"x\":1}", "not {"),
        ("a plus sign", "{\"version\":+2}", "not {"),
        ("no number", "{\"version\":}", "not {"),
        ("empty", "", "empty"),
        ("not utf-8", "\u{0}", "not {"),
    ] {
        store
            .put(&hint, body.as_bytes().to_vec().into())
            .await
            .unwrap();
        let err = pstore_format::lance::latest_manifest(&store, "lake/deleted")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(why), "{what}: {err}");
    }
    // Whitespace is fine; a version that has no manifest is an error from the store.
    store
        .put(&hint, b" { \"version\" : 2 } \n".to_vec().into())
        .await
        .unwrap();
    pstore_format::lance::latest_manifest(&store, "lake/deleted")
        .await
        .unwrap();
    store
        .put(&hint, b"{\"version\":99}".to_vec().into())
        .await
        .unwrap();
    assert!(
        pstore_format::lance::latest_manifest(&store, "lake/deleted")
            .await
            .is_err()
    );
    // Exactly 1 KiB is read (it is the same hint, padded); one byte more is refused unread.
    let mut padded = b"{\"version\":2}".to_vec();
    padded.resize(1024, b' ');
    store.put(&hint, padded.into()).await.unwrap();
    pstore_format::lance::latest_manifest(&store, "lake/deleted")
        .await
        .unwrap();
    // A hint over 1 KiB is refused unread; no hint at all is an error.
    store.put(&hint, vec![b' '; 1025].into()).await.unwrap();
    let err = pstore_format::lance::latest_manifest(&store, "lake/deleted")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("too large"), "{err}");
    assert!(
        pstore_format::lance::latest_manifest(&store, "lake/nope")
            .await
            .is_err()
    );
}

// ---- M101: scalar attribute columns ---------------------------------------------------------

const SCALARS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/lance-scalars/");

/// What `generate_lance_scalars.py` writes in column `col` at row `r`.
fn scalar(col: &str, r: i64) -> AttrValue {
    const DAY: i64 = 86_400_000_000;
    match col {
        "i32" => AttrValue::Int(r * 3 - 1500),
        "int64" => AttrValue::Int((r - 1500) << 40),
        "f32" => AttrValue::Float(f64::from(r as f32 / 4.0)),
        "f64" => AttrValue::Float(r as f64 / 8.0 - 100.0),
        "u64" => AttrValue::UInt((1u64 << 63) + r as u64),
        "date" => AttrValue::DateTime((r - 1500) * DAY),
        "ts_ms" => AttrValue::DateTime((r * 1000 - 7) * 1000),
        "ts_us" => AttrValue::DateTime(r - 5),
        "ts_ns" => AttrValue::DateTime((r * 1001 - 2000).div_euclid(1000)),
        "small" => AttrValue::Int(r),
        _ => unreachable!("{col}"),
    }
}

async fn read_scalar(name: &str, col: &str) -> (AttrKind, Vec<Option<AttrValue>>) {
    let (store, manifest) = dataset_in(SCALARS, name).await;
    let attr = ExternalAttribute::pin_lance(&store, &manifest, col)
        .await
        .unwrap_or_else(|e| panic!("{name}/{col}: {e}"));
    assert_eq!(attr.rows(), 2600, "{name}/{col}");
    (attr.kind, attr.read_all(&store).await.unwrap())
}

#[tokio::test]
async fn lance_scalar_columns_read_back_in_every_layout() {
    let kinds = [
        ("i32", AttrKind::Int32),
        ("int64", AttrKind::Int64),
        ("f32", AttrKind::Float),
        ("f64", AttrKind::Double),
        ("u64", AttrKind::UInt64),
        ("date", AttrKind::Date),
        ("ts_ms", AttrKind::TimestampMillis),
        ("ts_us", AttrKind::TimestampMicros),
        ("ts_ns", AttrKind::TimestampNanos),
    ];
    let deleted = 1000..1010;
    for name in ["one", "many", "v21"] {
        for (col, kind) in kinds {
            let (got_kind, got) = read_scalar(name, col).await;
            assert_eq!(got_kind, kind, "{name}/{col}");
            for (r, v) in got.iter().enumerate() {
                let want = (name != "many" || !deleted.contains(&r)).then(|| scalar(col, r as i64));
                assert_eq!(v.as_ref(), want.as_ref(), "{name}/{col} row {r}");
            }
        }
    }
    // The pin also follows the dataset's hint (M89).
    let (store, _) = dataset_in(SCALARS, "one").await;
    let attr = ExternalAttribute::pin_lance_latest(&store, "lake/one", "f64")
        .await
        .unwrap();
    assert_eq!(
        attr.read_all(&store).await.unwrap()[3],
        Some(scalar("f64", 3))
    );
}

#[tokio::test]
async fn a_lance_column_not_read_as_an_attribute_is_refused_by_name() {
    for (name, col, why) in [
        ("one", "b", "\"bool\""),
        ("one", "s", "\"string\""),
        ("one", "i16", "\"int16\""),
        ("one", "ts_s", "\"timestamp:s:-\""),
        ("one", "emb", "\"fixed_size_list:float:8\""),
        ("one", "nope", "no top-level column"),
        ("one", "i32n", "nulls"),
        ("v21", "i32n", "nulls"),
    ] {
        let (store, manifest) = dataset_in(SCALARS, name).await;
        let err = ExternalAttribute::pin_lance(&store, &manifest, col)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(why),
            "{name}/{col}: wanted {why:?}, got {err}"
        );
    }
    // The vector reader reads the same datasets' vectors, as before.
    for name in ["one", "many", "v21"] {
        let (store, manifest) = dataset_in(SCALARS, name).await;
        let col = ExternalColumn::pin_lance(&store, &manifest, "emb").await;
        assert!(col.is_ok(), "{name}: {col:?}");
    }
}

// ---- M102: Lance 2.1 bit-packed columns -----------------------------------------------------

/// The columns pylance 13 bit-packs in 2.1, with their kinds.
const PACKED: [(&str, AttrKind); 6] = [
    ("i32", AttrKind::Int32),
    ("date", AttrKind::Date),
    ("ts_ms", AttrKind::TimestampMillis),
    ("ts_us", AttrKind::TimestampMicros),
    ("ts_ns", AttrKind::TimestampNanos),
    ("small", AttrKind::Int32),
];

/// The rows `generate_lance_scalars.py` deletes from `v21-deleted`.
fn deleted_from_v21(r: usize) -> bool {
    (1000..1010).contains(&r) || [5, 1023, 1024, 1500, 2599].contains(&r)
}

#[tokio::test]
async fn bit_packed_lance_columns_read_back() {
    for name in ["v21", "v21-deleted"] {
        for (col, kind) in PACKED {
            let (store, manifest) = dataset_in(SCALARS, name).await;
            let attr = ExternalAttribute::pin_lance(&store, &manifest, col)
                .await
                .unwrap_or_else(|e| panic!("{name}/{col}: {e}"));
            let AttrSource::Fixed(column) = &attr.source else {
                panic!("{name}/{col}: not a fixed-width source");
            };
            // The fixtures exercise packing: in `v21` every run is a whole chunk, and in
            // `v21-deleted`, whose writer stored some fragments flat, a column mixes the two.
            let packed = column
                .extents
                .iter()
                .filter(|e| e.codec == CODEC_FASTLANES && e.compressed > 0)
                .count();
            let all = column.extents.len();
            let want = if name == "v21" {
                packed == all
            } else {
                packed > 0
            };
            assert!(want, "{name}/{col}: {packed} of {all} runs packed");
            assert_eq!((attr.kind, attr.rows()), (kind, 2600), "{name}/{col}");
            let got = attr.read_all(&store).await.unwrap();
            for (r, v) in got.iter().enumerate() {
                let gone = name == "v21-deleted" && deleted_from_v21(r);
                let want = (!gone).then(|| scalar(col, r as i64));
                assert_eq!(v.as_ref(), want.as_ref(), "{name}/{col} row {r}");
            }
        }
    }
}

/// `v21`'s `small`, with the first chunk's bytes at `at` (from the chunk's start) set to `to`.
async fn small_with(at: u64, to: u8) -> (MemoryStore, ExternalAttribute) {
    let (store, manifest) = dataset_in(SCALARS, "v21").await;
    let attr = ExternalAttribute::pin_lance(&store, &manifest, "small")
        .await
        .unwrap();
    let AttrSource::Fixed(column) = &attr.source else {
        panic!("not a fixed-width source");
    };
    let key = Key::new(column.key.clone());
    let mut bytes = store.get(&key).await.unwrap().to_vec();
    bytes[(column.extents[0].offset + at) as usize] = to;
    store.put(&key, bytes.into()).await.unwrap();
    let attr = ExternalAttribute::pin_lance(&store, &manifest, "small")
        .await
        .unwrap();
    (store, attr)
}

#[tokio::test]
async fn a_damaged_packed_chunk_is_refused_not_misread() {
    // The chunk's header: a level byte, and a values length that disagrees with the chunk.
    // Then its bit width, which the values are then not sized for: 33, past the value's, and 1.
    for (at, to, why) in [
        (0, 1, "levels"),
        (2, 0, "values length"),
        (8, 33, "33 bits"),
        (8, 1, "a width"),
    ] {
        let (store, attr) = small_with(at, to).await;
        let err = attr.read_all(&store).await.unwrap_err();
        assert!(matches!(err, ExternalError::Damaged), "{why}: {err}");
    }
    // An object changed after the pin, to the same length.
    let (store, manifest) = dataset_in(SCALARS, "v21").await;
    let attr = ExternalAttribute::pin_lance(&store, &manifest, "small")
        .await
        .unwrap();
    let AttrSource::Fixed(column) = &attr.source else {
        panic!("not a fixed-width source");
    };
    let key = Key::new(column.key.clone());
    let mut bytes = store.get(&key).await.unwrap().to_vec();
    bytes[column.extents[0].offset as usize + 20] ^= 1;
    store.put(&key, bytes.into()).await.unwrap();
    let err = attr.read_all(&store).await.unwrap_err();
    assert!(matches!(err, ExternalError::Changed), "{err}");
}
