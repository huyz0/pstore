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
use pstore_format::external::{ExternalColumn, ExternalError};
use pstore_format::lance::{LanceError, locate};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/lance/");

/// Every file of the dataset directory `name`, stored under `lake/<name>/...`.
async fn dataset(name: &str) -> (MemoryStore, Key) {
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
    let root = std::path::PathBuf::from(format!("{DIR}{name}"));
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
    refused("many", "3 fragments").await;
    refused("deleted", "deleted rows").await;
    refused("nullable", "null-free float32").await;
    refused("v21", "format 2.1").await;
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
    rejected(&with(&|s| s.file_version = (2, 1)), "format 2.1").await;
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
