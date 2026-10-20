//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M90: a float32 vector column of a Lance 2.1 dataset, read in place.
//!
//! Real pylance 13 datasets (written by `data/generate_lance21.py`): the two layouts a null-free
//! vector column takes (`MiniBlock` under 256 bytes a row, `FullZip` above) and the shapes
//! around chunk boundaries.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::external::ExternalColumn;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/lance21/");

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
        // Joined with `/`: on Windows `to_str()` gives `\`, and a key is never a
        // Windows path (M112).
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_str().unwrap())
            .collect::<Vec<_>>()
            .join("/");
        let key = Key::new(format!("lake/{name}/{rel}"));
        store
            .put(&key, std::fs::read(&f).unwrap().into())
            .await
            .unwrap();
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

fn floats(name: &str) -> Vec<f32> {
    std::fs::read(format!("{DIR}{name}.f32"))
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

#[tokio::test]
async fn a_lance_2_1_dataset_reads_back_every_row() {
    // (name, rows, dimensions, chunks or pages the column is cut into)
    for (name, rows, dims, runs) in [
        ("one", 1usize, 3usize, 1usize),
        ("padded", 129, 3, 1),
        ("exact", 256, 8, 2),
        ("many", 2600, 8, 21),
        ("full", 300, 100, 1),
    ] {
        let (store, manifest) = dataset(name).await;
        let col = ExternalColumn::pin_lance(&store, &manifest, "emb")
            .await
            .unwrap();
        assert_eq!(
            (col.dims as usize, col.rows as usize),
            (dims, rows),
            "{name}"
        );
        assert_eq!(col.extents.len(), runs, "{name}");
        let want = floats(name);
        let all: Vec<usize> = (0..rows).collect();
        let got = col.read_rows(&store, &all).await.unwrap();
        assert_eq!(got.len(), rows, "{name}");
        for (r, v) in &got {
            assert_eq!(
                v.as_slice(),
                &want[r * dims..(r + 1) * dims],
                "{name} row {r}"
            );
        }
    }
}

#[tokio::test]
async fn a_2_1_column_with_nulls_is_refused_by_name() {
    let (store, manifest) = dataset("nulls").await;
    let err = ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not a null-free, uncompressed run"),
        "{err}"
    );
}

/// `name`, with the last `find` in the data file replaced by `with` (the same length). The last,
/// because the metadata follows the data, and `emb`'s follows `id`'s.
async fn patched(name: &str, find: &[u8], with: &[u8]) -> (MemoryStore, Key) {
    assert_eq!(find.len(), with.len());
    let (store, manifest) = dataset(name).await;
    let dir = std::fs::read_dir(format!("{DIR}{name}/data")).unwrap();
    let file = dir.into_iter().next().unwrap().unwrap().file_name();
    let key = Key::new(format!("lake/{name}/data/{}", file.to_str().unwrap()));
    let mut raw = std::fs::read(format!("{DIR}{name}/data/{}", file.to_str().unwrap())).unwrap();
    let at = (0..raw.len() - find.len())
        .rev()
        .find(|i| &raw[*i..*i + find.len()] == find)
        .expect("the bytes to patch are in the file");
    raw[at..at + find.len()].copy_from_slice(with);
    store.put(&key, raw.into()).await.unwrap();
    (store, manifest)
}

async fn refusal(name: &str, find: &[u8], with: &[u8]) -> String {
    let (store, manifest) = patched(name, find, with).await;
    ExternalColumn::pin_lance(&store, &manifest, "emb")
        .await
        .unwrap_err()
        .to_string()
}

#[tokio::test]
async fn a_damaged_chunk_index_is_refused_by_name() {
    // `exact` is two chunks of 128 rows of 32 bytes: index entries `07 20 07 20`, each 512
    // words (4096 bytes) of 2^7 values. `padded` is one short chunk: `98 0c`.
    let idx = [0x07, 0x20, 0x07, 0x20];
    for (name, find, with, why) in [
        // words one too many; one too few
        (
            "exact",
            &idx[..],
            [0x17, 0x20, 0x07, 0x20],
            "padded to 8 bytes",
        ),
        (
            "exact",
            &idx[..],
            [0x07, 0x20, 0x17, 0x20],
            "padded to 8 bytes",
        ),
        // the first of two chunks declares no size, and a size that is not the page's
        (
            "exact",
            &idx[..],
            [0x00, 0x20, 0x07, 0x20],
            "declares no size",
        ),
        (
            "exact",
            &idx[..],
            [0x07, 0x20, 0x06, 0x20],
            "disagrees with the page",
        ),
        // a first chunk that takes every row leaves the last none
        ("exact", &idx[..], [0x08, 0x40, 0x00, 0x00], "holds no rows"),
    ] {
        let err = refusal(name, find, &with).await;
        assert!(
            err.contains(why),
            "{name} {with:02x?}: wanted {why:?}, got {err}"
        );
    }
}

#[tokio::test]
async fn a_page_whose_chunks_do_not_fill_its_buffer_is_refused() {
    // `exact`'s page sizes are `04 90 40`: an index of 4 bytes and 8,208 of chunks.
    let err = refusal("exact", &[0x04, 0x90, 0x40], &[0x04, 0x98, 0x40]).await;
    assert!(err.contains("do not hold its rows and bytes"), "{err}");
    let err = refusal("exact", &[0x04, 0x90, 0x40], &[0x04, 0x88, 0x40]).await;
    assert!(err.contains("do not hold its rows and bytes"), "{err}");
}

#[tokio::test]
async fn a_layout_that_is_not_a_plain_null_free_run_is_refused_by_name() {
    let what = "not a null-free, uncompressed run";
    // `exact` is MiniBlock: `... 5a 08 08 08 12 04 0a 02 08 20 | 32 01 01 | 38 01 | 48 80 02`
    for (find, with) in [
        // the items the layout claims differ from the page's rows
        (
            &[0x38, 0x01, 0x48, 0x80, 0x02][..],
            &[0x38, 0x01, 0x48, 0x81, 0x02][..],
        ),
        // the item has nulls (a layer other than all-valid); two buffers a chunk
        (&[0x32, 0x01, 0x01], &[0x32, 0x01, 0x03]),
        (&[0x38, 0x01, 0x48], &[0x38, 0x02, 0x48]),
        // eight dimensions become nine; 32 bits a value become 16
        (&[0x5a, 0x08, 0x08, 0x08], &[0x5a, 0x08, 0x08, 0x09]),
        (
            &[0x0a, 0x02, 0x08, 0x20, 0x32],
            &[0x0a, 0x02, 0x08, 0x10, 0x32],
        ),
    ] {
        let err = refusal("exact", find, with).await;
        assert!(err.contains(what), "{find:02x?}: {err}");
    }
    // `full` is FullZip: `18 80 19 | 28 ac 02 | 30 ac 02 | 3a 0a 5a 08 08 64 ... | 42 01 01`
    for (find, with) in [
        // 3,200 bits a value become 3,208; visible and total items differ from the rows
        (&[0x18, 0x80, 0x19][..], &[0x18, 0x88, 0x19][..]),
        (&[0x28, 0xac, 0x02], &[0x28, 0xad, 0x02]),
        (&[0x30, 0xac, 0x02], &[0x30, 0xad, 0x02]),
        // the item has nulls; 100 dimensions become 101
        (&[0x42, 0x01, 0x01], &[0x42, 0x01, 0x03]),
        (&[0x5a, 0x08, 0x08, 0x64], &[0x5a, 0x08, 0x08, 0x65]),
    ] {
        let err = refusal("full", find, with).await;
        assert!(err.contains(what), "{find:02x?}: {err}");
    }
}

#[tokio::test]
async fn a_full_zip_page_must_be_exactly_its_rows() {
    // `full`'s buffer is `12 03 c0 a9 07`: 120,000 bytes = 300 x 100 x 4, at 1,280 in a file of 121,686.
    let size = [0xc0, 0xa9, 0x07];
    let err = refusal("full", &size, &[0xc1, 0xa9, 0x07]).await;
    assert!(err.contains("not rows x dims x 4"), "{err}");
    // A buffer ending exactly at the end of the file is inside it, one byte more is not.
    let err = refusal("full", &size, &[0xd6, 0xac, 0x07]).await;
    assert!(err.contains("not rows x dims x 4"), "{err}");
    let err = refusal("full", &size, &[0xd7, 0xac, 0x07]).await;
    assert!(err.contains("run past the file"), "{err}");
}

#[tokio::test]
async fn a_chunk_index_of_an_odd_length_is_refused() {
    // `exact`'s sizes are `04 90 40`; an index of 5 bytes reads its fifth as half an entry.
    let err = refusal("exact", &[0x04, 0x90, 0x40], &[0x05, 0x90, 0x40]).await;
    assert!(err.contains("16-bit entries"), "{err}");
}
