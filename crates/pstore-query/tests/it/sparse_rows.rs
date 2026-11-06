//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M193: a page's sparse pairs read from `SparseRows`, and what is refused when they cannot
//! be.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::sparse::{DEFAULT_ENCODING, build};
use pstore_format::{Document, Impact, Section, Segment, SegmentWriter, VectorField};
use pstore_query::{Projection, QueryError, Target, vectors_at};

fn docs() -> Vec<Document> {
    (0..3)
        .map(|i| {
            let mut d = Document::new(format!("d{i}"), vec![1.0, 0.0]);
            d.vectors.insert(
                "terms".into(),
                VectorField::Sparse(vec![
                    ("b".to_owned(), Impact::new(0.5)),
                    ("a".to_owned(), Impact::new(-1.0)),
                ]),
            );
            d
        })
        .collect()
}

/// A segment of `docs()` with `rows` as its `SparseRows`, or none.
async fn open(store: &MemoryStore, name: &str, rows: Option<Vec<u8>>) -> (Target, Segment) {
    let docs = docs();
    let p = build(&docs, "terms", DEFAULT_ENCODING);
    let mut w = SegmentWriter::new(2);
    for d in docs {
        w.push(d);
    }
    let mut w = w.with_section(Section::SparsePostings, p.section);
    if let Some(rows) = rows {
        w = w.with_section(Section::SparseRows, rows);
    }
    let key = Key::new(format!("t/i/{name}.seg"));
    store.put(&key, w.try_finish().unwrap()).await.unwrap();
    let seg = Segment::open(store, &key).await.unwrap();
    let t = Target {
        segment_len: None,
        centroids: None,
        deleted: None,
        sparse_dict: false,
        text_dict: false,
        fde_clusters: false,
        shadowed: false,
        segment: key,
    };
    (t, seg)
}

async fn page(
    store: &MemoryStore,
    t: Target,
    seg: Segment,
) -> Result<
    std::collections::BTreeMap<(usize, usize), std::collections::BTreeMap<String, VectorField>>,
    QueryError,
> {
    let projection = Projection {
        sparse: true,
        ..Projection::default()
    };
    vectors_at(store, &[t], &[seg], &[(0, 0), (0, 2)], projection).await
}

#[tokio::test]
async fn a_pages_pairs_are_read_and_unreadable_ones_refused() {
    let store = MemoryStore::new();
    // As a fold writes it: each row's pairs, in key order.
    let (t, seg) = open(
        &store,
        "ok",
        Some(build(&docs(), "terms", DEFAULT_ENCODING).rows),
    )
    .await;
    let got = page(&store, t, seg).await.unwrap();
    for row in [0, 2] {
        let VectorField::Sparse(pairs) = &got[&(0, row)]["terms"] else {
            panic!("row {row}: {got:?}");
        };
        let pairs: Vec<(&str, f32)> = pairs.iter().map(|(k, w)| (k.as_str(), w.get())).collect();
        assert_eq!(pairs, [("a", -1.0), ("b", 0.5)], "row {row}");
    }

    // Folded before M193: refused, naming the field.
    let (t, seg) = open(&store, "old", None).await;
    let e = page(&store, t, seg).await.unwrap_err();
    assert!(
        matches!(&e, QueryError::SparseUnstored(f) if f == "terms"),
        "{e:?}"
    );

    // A row's range that is not whole pairs (a key of 5 bytes, 1 present) is corrupt, never
    // an empty row.
    let mut bad = Vec::new();
    for o in [0u64, 3, 3, 3] {
        bad.extend_from_slice(&o.to_le_bytes());
    }
    bad.extend_from_slice(&[5, 0, b'a']);
    let (t, seg) = open(&store, "bad", Some(bad)).await;
    let e = page(&store, t, seg).await.unwrap_err();
    assert!(
        matches!(
            &e,
            QueryError::Format(pstore_format::FormatError::Corrupt(_))
        ),
        "{e:?}"
    );
}
