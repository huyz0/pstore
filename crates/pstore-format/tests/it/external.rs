//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M68: vectors read in place out of an external, pinned object.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::external::{ExternalColumn, ExternalError};
use pstore_types::TenantId;

const HEADER: usize = 16;
const DIMS: usize = 4;
const STRIDE: usize = 24; // 16 bytes of vector, 8 of something else
const ROWS: usize = 200_000;
const TRAILER: usize = 40;

fn value(row: usize, d: usize) -> f32 {
    (row * 10 + d) as f32 + 0.5
}

fn file() -> Vec<u8> {
    let mut b = vec![0xAA; HEADER];
    for r in 0..ROWS {
        for d in 0..DIMS {
            b.extend(value(r, d).to_le_bytes());
        }
        b.extend([0xBB; STRIDE - DIMS * 4]);
    }
    b.extend([0xCC; TRAILER]);
    b
}

fn key() -> Key {
    Key::new("lake/vectors.bin")
}

fn column(len: u64, rows: u64) -> ExternalColumn {
    ExternalColumn {
        key: "lake/vectors.bin".to_owned(),
        base: HEADER as u64,
        dims: DIMS as u32,
        stride: STRIDE as u64,
        rows,
        len,
        tag: None,
        extents: Vec::new(),
        more: Vec::new(),
    }
}

#[test]
fn row_ranges_are_computed() {
    let c = column(1 << 20, 10);
    assert_eq!(c.row_range(0), Some(16..32));
    assert_eq!(c.row_range(3), Some(88..104));
    assert_eq!(c.row_range(9), Some(232..248));
    assert_eq!(c.row_range(10), None);
    assert_eq!(c.row_range(usize::MAX), None);
}

#[tokio::test]
async fn pin_refuses_a_column_that_does_not_fit() {
    let store = MemoryStore::new();
    let bytes = file();
    let len = bytes.len() as u64;
    store.put(&key(), bytes.into()).await.unwrap();
    let pin = |base: u64, dims: u32, stride: u64, rows: u64| {
        let store = store.clone();
        async move { ExternalColumn::pin(&store, &key(), base, dims, stride, rows).await }
    };
    let ok = pin(HEADER as u64, 4, STRIDE as u64, ROWS as u64)
        .await
        .unwrap();
    assert_eq!(ok.len, len);
    assert!(pin(HEADER as u64, 0, 24, 10).await.is_err(), "zero dims");
    assert!(
        pin(HEADER as u64, 4, 15, 10).await.is_err(),
        "stride under the vector"
    );
    assert!(
        pin(HEADER as u64, 4, STRIDE as u64, ROWS as u64 + 3)
            .await
            .is_err(),
        "past the end"
    );
    assert!(pin(u64::MAX - 8, 4, 24, 10).await.is_err(), "overflow");
    assert!(
        pin(HEADER as u64, 4, u64::MAX, 3).await.is_err(),
        "overflowing stride"
    );
    // The exact fit: the last row ends before the trailer, and zero rows is empty.
    assert!(
        pin(HEADER as u64, 4, STRIDE as u64, ROWS as u64)
            .await
            .is_ok()
    );
    assert!(pin(HEADER as u64, 4, STRIDE as u64, 0).await.is_ok());
    // A row that ends exactly at the object's end is accepted, one byte more is not.
    let tight = pin(len - 16, 4, 16, 1).await;
    assert!(tight.is_ok(), "{tight:?}");
    assert!(pin(len - 15, 4, 16, 1).await.is_err());
}

#[tokio::test]
async fn read_rows_returns_the_files_floats() {
    let store = MemoryStore::new();
    store.put(&key(), file().into()).await.unwrap();
    let c = ExternalColumn::pin(&store, &key(), HEADER as u64, 4, STRIDE as u64, ROWS as u64)
        .await
        .unwrap();
    let got = c
        .read_rows(&store, &[7, 0, 199_999, 7, 200_000, 500_000])
        .await
        .unwrap();
    assert_eq!(got.len(), 3, "duplicates and past-the-end rows dropped");
    for r in [0usize, 7, 199_999] {
        let want: Vec<f32> = (0..DIMS).map(|d| value(r, d)).collect();
        assert_eq!(got[&r], want, "row {r}");
    }
    assert!(c.read_rows(&store, &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn read_rows_reads_only_the_rows() {
    let acct = Accounted::new(MemoryStore::new());
    let store = acct.as_tenant(TenantId(1));
    store.put(&key(), file().into()).await.unwrap();
    let c = ExternalColumn::pin(&store, &key(), HEADER as u64, 4, STRIDE as u64, ROWS as u64)
        .await
        .unwrap();
    acct.record_ranges();
    let got = c.read_rows(&store, &[3, 100_000, 199_998]).await.unwrap();
    assert_eq!(got.len(), 3);
    let moved = acct.bytes_in(&key(), 0..c.len);
    // Three rows of 16 bytes, however the coalescer merges, and nowhere near the file.
    assert!(moved >= 48, "{moved}");
    assert!(moved * 20 < c.len, "{moved} of {}", c.len);
}

#[tokio::test]
async fn a_changed_object_is_refused() {
    let store = MemoryStore::new();
    store.put(&key(), file().into()).await.unwrap();
    let c = ExternalColumn::pin(&store, &key(), HEADER as u64, 4, STRIDE as u64, ROWS as u64)
        .await
        .unwrap();
    assert!(c.read_rows(&store, &[1]).await.is_ok());
    // Same length, other bytes: the tag moves.
    let mut other = file();
    other[HEADER] ^= 0xFF;
    store.put(&key(), other.into()).await.unwrap();
    assert!(matches!(
        c.read_rows(&store, &[1]).await,
        Err(ExternalError::Changed)
    ));
    // A different length.
    let mut longer = file();
    longer.push(0);
    store.put(&key(), longer.into()).await.unwrap();
    assert!(matches!(
        c.read_rows(&store, &[1]).await,
        Err(ExternalError::Changed)
    ));
    // Gone altogether is an error, not an empty answer.
    store.delete_batch(&[key()]).await.unwrap();
    assert!(c.read_rows(&store, &[1]).await.is_err());
    // Nothing asked is nothing read, even of a changed object.
    assert!(c.read_rows(&store, &[]).await.is_ok());
}

#[tokio::test]
async fn pins_are_checked_in_one_round() {
    // M120: a column over three objects checks every pin together, then reads its rows: two
    // rounds. One object after another, a `head` then a `get_tag` each, it was seven.
    let store = pstore_testkit::depth::DepthCounting::new(MemoryStore::new());
    store.put(&key(), file().into()).await.unwrap();
    let mut c = ExternalColumn::pin(&store, &key(), HEADER as u64, 4, STRIDE as u64, ROWS as u64)
        .await
        .unwrap();
    for name in ["lake/more-1.bin", "lake/more-2.bin"] {
        let k = Key::new(name);
        store.put(&k, vec![0u8; 64].into()).await.unwrap();
        c.more.push(pstore_format::external::ObjectRef {
            key: name.to_owned(),
            len: 64,
            tag: store.get_tag(&k).await.unwrap(),
        });
    }
    store.reset();
    let got = c.read_rows(&store, &[3, 9]).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(store.depth(), 2, "pins then rows");
    assert_eq!(
        store.requests(),
        7,
        "a head and a tag an object, then one read"
    );
    // A pin that moved is still refused, and the main object's first, as before.
    store
        .put(&Key::new("lake/more-2.bin"), vec![0u8; 65].into())
        .await
        .unwrap();
    assert!(matches!(
        c.read_rows(&store, &[3]).await,
        Err(ExternalError::Changed)
    ));
}
