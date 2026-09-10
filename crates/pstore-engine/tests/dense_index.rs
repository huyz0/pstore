//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The fold builds a dense index — M5g.1.
//!
//! ⚠️ **The failure is that nothing fails.** `Engine::search` scans every document of every
//! segment and scores them in memory, exactly — so the ANN index, the RaBitQ codes, the `Sq8`
//! rerank rung and the centroid table are all built, tested, gated on recall, and **never
//! reached from a write the engine performed**.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::Engine;
use pstore_format::{Document, Impact, Section, Segment, Value, VectorField};
use pstore_index::vec_index;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const DIM: usize = 8;

fn doc(i: usize) -> Document {
    Document::new(
        format!("d{i:05}"),
        (0..DIM).map(|j| ((i + j) % 13) as f32 / 13.0).collect(),
    )
}

/// Every segment HEAD names for `index`, opened.
async fn segments(store: &Arc<MemoryStore>, t: TenantId, index: &str) -> Vec<(Key, Segment)> {
    let e = Engine::new(Arc::clone(store), t, LaneId(9));
    let head = e.head_for_test().await;
    let mut out = Vec::new();
    for r in head.indexes.get(index).cloned().unwrap_or_default() {
        let k = Key::new(r.key);
        let seg = Segment::open(store.as_ref(), &k).await.unwrap();
        out.push((k, seg));
    }
    out
}

/// Folds `docs` in one go, through an engine with a small exact-scan threshold so the test
/// does not have to write 25,000 rows to reach the clustered path.
async fn fold_with(store: &Arc<MemoryStore>, t: TenantId, docs: Vec<Document>, threshold: usize) {
    let e = Engine::new(Arc::clone(store), t, LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            target_list_size: 40,
            exact_scan_threshold: threshold,
            ..pstore_index::cluster::Params::default()
        },
    );
    e.write("idx", docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

#[tokio::test]
async fn a_folded_segment_carries_its_dense_index() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(700);
    fold_with(&store, t, (0..300).map(doc).collect(), 100).await;

    let segs = segments(&store, t, "idx").await;
    assert_eq!(segs.len(), 1);
    let (key, seg) = &segs[0];
    assert!(
        seg.section(Section::RaBitQ).is_some(),
        "a folded segment carries no 1-bit codes, so every query is an exact scan"
    );
    assert!(seg.section(Section::Sq8).is_some(), "no rerank rung");
    assert!(
        store.get(&vec_index::centroid_key(key)).await.is_ok(),
        "the centroid table was built and never written, so the index is unreachable"
    );
}

#[tokio::test]
async fn below_the_threshold_no_centroid_object_is_written() {
    // ⚠️ D-10: an index below the exact-scan threshold says "scan me exactly" by the centroid
    // object being ABSENT. Writing one anyway is a request and an object per fold for a table
    // no query reads.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(701);
    fold_with(&store, t, (0..20).map(doc).collect(), 100).await;

    let segs = segments(&store, t, "idx").await;
    let (key, seg) = &segs[0];
    assert!(store.get(&vec_index::centroid_key(key)).await.is_err());
    assert_eq!(
        seg.row_count(),
        20,
        "the rows went missing along with the index"
    );
}

#[tokio::test]
async fn a_compaction_rebuilds_the_dense_index() {
    // ⚠️ Otherwise the first merge silently un-indexes an index: every row present, every
    // query exact, and nothing reporting anything.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(702);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            target_list_size: 40,
            exact_scan_threshold: 100,
            ..pstore_index::cluster::Params::default()
        },
    );
    for batch in 0..2 {
        e.write("idx", (batch * 200..batch * 200 + 200).map(doc).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    assert_eq!(segments(&store, t, "idx").await.len(), 2);

    e.compact("idx").await.unwrap();
    let segs = segments(&store, t, "idx").await;
    assert_eq!(segs.len(), 1);
    let (key, seg) = &segs[0];
    assert_eq!(seg.row_count(), 400);
    assert!(
        seg.section(Section::RaBitQ).is_some(),
        "the merge produced a segment with no dense index"
    );
    assert!(
        store.get(&vec_index::centroid_key(key)).await.is_ok(),
        "the merged segment's centroid table is missing, so its index is unreachable"
    );
}

#[tokio::test]
async fn a_hybrid_corpus_survives_the_reordering() {
    // ⚠️ The only place the row REORDERING meets the two sidecars the engine used to build
    // itself. The clustering decides the segment's row order, and both the sparse postings and
    // the text postings and fieldnorms address those rows — three builders called over the
    // input order produce three internally consistent indexes pointing at three different
    // documents. Asserted by document id, because the row order changes by construction.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(703);
    let docs: Vec<Document> = (0..300)
        .map(|i| {
            let mut d = doc(i);
            d.vectors.insert(
                "body_sparse".to_owned(),
                VectorField::Sparse(vec![(i as u32 % 17, Impact::new(0.9))]),
            );
            d.attrs.insert(
                pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
                Value::Str(format!("document {i} quarterly revenue w{}", i % 29)),
            );
            d
        })
        .collect();
    fold_with(&store, t, docs.clone(), 100).await;

    let segs = segments(&store, t, "idx").await;
    let (key, seg) = &segs[0];
    let rows = seg.scan(store.as_ref(), key, None).await.unwrap();
    assert_eq!(rows.len(), 300);

    // ⚠️ The row order DID change — otherwise this test is checking nothing.
    let ids: Vec<&str> = rows.iter().map(|d| d.id.as_str()).collect();
    let input: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
    assert_ne!(
        ids, input,
        "nothing was clustered, so the fixture is not testing the reordering"
    );
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    let mut want = input.clone();
    want.sort_unstable();
    assert_eq!(sorted, want, "the reordering lost or duplicated a document");

    // Both sidecars are beside the segment, and both address the segment's rows.
    assert!(store.get(&pstore_format::text::dict_key(key)).await.is_ok());
    assert!(
        store
            .get(&pstore_format::sparse::dict_key(key))
            .await
            .is_ok()
    );
    assert_eq!(seg.text_fields(), ["text"]);
    assert_eq!(seg.sparse_field(), Some("body_sparse"));

    // Each document's own fields came back with it, which is what a mis-ordered posting or
    // fieldnorm breaks.
    for got in &rows {
        let want = docs.iter().find(|d| d.id == got.id).expect("id");
        assert_eq!(got.vector(), want.vector(), "{} lost its vector", got.id);
        assert_eq!(
            got.attrs.get(pstore_format::text::DEFAULT_TEXT_FIELD),
            want.attrs.get(pstore_format::text::DEFAULT_TEXT_FIELD),
            "{} lost its text",
            got.id
        );
    }
}

#[tokio::test]
async fn a_vector_only_fold_writes_no_sidecars() {
    // A `.tdict` per fold for an index with no prose is a PUT and an object that never reads
    // back.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(704);
    fold_with(&store, t, (0..300).map(doc).collect(), 100).await;
    let segs = segments(&store, t, "idx").await;
    let key = &segs[0].0;
    assert!(
        store
            .get(&pstore_format::text::dict_key(key))
            .await
            .is_err()
    );
    assert!(
        store
            .get(&pstore_format::sparse::dict_key(key))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_replicated_vector_is_still_one_row() {
    // ⚠️ Boundary replication puts a vector into a second posting list, and rows are written
    // in list order — so a replicated document would be written to the segment TWICE. Harmless
    // for a read-only fixture; wrong for a durable segment, because `Engine::scan` promises
    // every row "exactly once" and a compaction re-seals what it scanned, so it compounds on
    // every merge. Measured before the clamp: 400 documents folded and merged came back as
    // **431 rows**.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(705);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            target_list_size: 40,
            exact_scan_threshold: 100,
            replicas: 4,
            boundary: 0.9,
            ..pstore_index::cluster::Params::default()
        },
    );
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let segs = segments(&store, t, "idx").await;
    assert_eq!(
        segs[0].1.row_count(),
        300,
        "replication duplicated rows in a durable segment"
    );
    assert_eq!(
        e.scan("idx", None).await.unwrap().len(),
        300,
        "scan returned a document more than once"
    );

    // And it does not compound: a merge of two such segments is the sum, not more.
    e.write("idx", (300..600).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.compact("idx").await.unwrap();
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 600);
}

#[tokio::test]
async fn an_over_wide_segment_is_refused_by_the_fold() {
    // ⚠️ The refusal `try_build_all` exists for, and it is NOT `check_storable`'s: that guards
    // the write door against documents the format cannot hold, and it knows nothing about how
    // wide the resulting segment is. `try_finish` refuses a segment whose meta region does not
    // fit the suffix read — because such a segment opens in two round trips on every query,
    // for the life of an immutable object. `build_all`'s `finish` writes it and reports it
    // durable.
    //
    // Zone maps are per block and per attribute KEY, so enough distinct int attributes make
    // even a one-block index exceed `INDEX_BUDGET`.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(706);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    let wide: Vec<Document> = (0..4)
        .map(|i| {
            let mut d = doc(i);
            for k in 0..400 {
                d.attrs
                    .insert(format!("attribute_number_{k:04}"), Value::Int(i as i64 + k));
            }
            d
        })
        .collect();
    e.write("idx", wide).await.unwrap();
    e.flush().await.unwrap();

    let err = e
        .fold()
        .await
        .expect_err("a segment too wide to open in one round trip was folded and committed");
    assert!(
        format!("{err}").contains("round trip"),
        "the refusal did not name what was wrong: {err}"
    );
    // ⚠️ And nothing was committed: a refused fold must not name a segment it did not write.
    let e2 = Engine::new(Arc::clone(&store), t, LaneId(2));
    assert!(
        e2.head_for_test().await.indexes.is_empty(),
        "the refused fold committed a HEAD anyway"
    );
}
