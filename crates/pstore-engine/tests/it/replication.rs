//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Replication is correct now, so the engine stops clamping it — M3c.3.
//!
//! ⚠️ **This turns nothing on.** `Engine`'s own default stays `replicas: 0`, because at the
//! default probe width of 8 replication buys **0.0000** recall and costs bytes — measured by
//! `cargo run --release -p pstore-index --example recall -- --replicas`. What changes is that a
//! caller who lowers `p` can now take the trade: 0.9610 @ 0.288 MB/query at p=2 against
//! 0.9680 @ 0.429 MB unreplicated at p=4.
//!
//! The clamp existed because replication was **wrong**: a replicated vector was written to the
//! segment twice, so 400 documents folded and merged came back as 431 rows and compounded on
//! every merge, against `Engine::scan`'s promise of "exactly once".

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, Segment, Value};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const DIM: usize = 8;

/// ⚠️ Pseudorandom, not a lattice. The first fixture used `(i * 7 + j * 13) % 97`, whose
/// points sit on a regular grid — every one of them is decisively closer to one centroid than
/// to any other, so `augment` replicated **nothing** and the fixture tested the opposite of
/// what it claimed. Replication exists for vectors *between* centroids.
fn doc(i: usize) -> Document {
    let mut st = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        (st >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let mut d = Document::new(format!("d{i:05}"), (0..DIM).map(|_| next()).collect());
    d.attrs.insert(
        pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str(format!("document {i} quarterly revenue w{}", i % 29)),
    );
    d
}

fn replicating() -> pstore_index::cluster::Params {
    pstore_index::cluster::Params {
        target_list_size: 40,
        exact_scan_threshold: 100,
        replicas: 4,
        boundary: 0.9,
        ..pstore_index::cluster::Params::default()
    }
}

async fn segments(store: &Arc<MemoryStore>, t: TenantId) -> Vec<(Key, Segment)> {
    let e = pstore_engine::Engine::new(Arc::clone(store), t, LaneId(9));
    let head = e.head_for_test().await;
    let mut out = Vec::new();
    for r in head.indexes.get("idx").cloned().unwrap_or_default() {
        let k = Key::new(r.key);
        let seg = Segment::open(&**store, &k).await.unwrap();
        out.push((k, seg));
    }
    out
}

#[tokio::test]
async fn a_replicated_segment_holds_each_document_once() {
    // ⚠️ The criterion M5g's clamp exists to satisfy, now met with replication ON.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(900);
    let e = pstore_engine::Engine::new(Arc::clone(&store), t, LaneId(1))
        .with_index_params(replicating());
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let segs = segments(&store, t).await;
    assert_eq!(segs[0].1.row_count(), 300, "replication duplicated rows");
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 300);

    // And it does not compound: a merge of two such segments is the sum, not more.
    e.write("idx", (300..600).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.compact("idx").await.unwrap();
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 600);
    assert_eq!(segments(&store, t).await[0].1.row_count(), 600);
}

#[tokio::test]
async fn the_codes_carry_the_replicas() {
    // ⚠️ Without this, the test above passes over a build that silently dropped replication
    // and the milestone buys nothing.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(901);
    let e = pstore_engine::Engine::new(Arc::clone(&store), t, LaneId(1))
        .with_index_params(replicating());
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let segs = segments(&store, t).await;
    let seg = &segs[0].1;
    assert!(
        seg.index_row_count() > seg.row_count(),
        "the codes cover {} rows for {} documents, so nothing was replicated",
        seg.index_row_count(),
        seg.row_count()
    );
    assert!(seg.section(Section::IndexRows).is_some());
    let map = seg.index_rows(store.as_ref(), &segs[0].0).await.unwrap();
    assert_eq!(map.len(), seg.index_row_count());
    assert!(
        map.iter().all(|r| (*r as usize) < seg.row_count()),
        "the mapping names a row the blocks do not have"
    );
}

#[tokio::test]
async fn replication_does_not_change_the_bm25_statistics() {
    // ⚠️ The defect live since M3: the sidecars were built over the EXPANDED rows, so a
    // replicated document was counted twice in `doc_count` and in every one of its terms'
    // `df` — a corpus scored against statistics saying it is bigger than it is. The recall
    // gate builds no text field and the ndcg gate does not replicate, so nothing caught it.
    let docs: Vec<Document> = (0..300).map(doc).collect();
    let mut summaries = Vec::new();
    for params in [
        pstore_index::cluster::Params {
            replicas: 0,
            boundary: 0.0,
            ..replicating()
        },
        replicating(),
    ] {
        let store = Arc::new(MemoryStore::new());
        let key = Key::new("seg");
        let built = pstore_index::vec_index::build_all(
            &docs,
            params,
            pstore_format::DEFAULT_FIELD,
            None,
            Some(pstore_format::text::DEFAULT_TEXT_FIELD),
        );
        store.put(&key, built.segment).await.unwrap();
        store
            .put(
                &pstore_format::text::dict_key(&key),
                bytes::Bytes::from(built.text_dictionary.expect("a dictionary")),
            )
            .await
            .unwrap();
        let idx = pstore_index::text::TextIndex::open(&*store, &key)
            .await
            .unwrap();
        summaries.push(idx.summary());
    }
    assert_eq!(
        summaries[0].doc_count, summaries[1].doc_count,
        "replication changed the corpus size BM25 is scored against"
    );
    assert_eq!(
        summaries[0].total_tokens, summaries[1].total_tokens,
        "replication changed avgdl"
    );
    assert_eq!(
        summaries[0].df, summaries[1].df,
        "replication changed a term's document frequency"
    );
}

#[tokio::test]
async fn a_replicated_document_appears_once_in_a_top_k() {
    // ⚠️ **Written because a mutation survived.** Removing the deduplication in
    // `VecIndex::search` changed nothing, because nothing here probed a replicated index and
    // looked at the ranking. A vector replicated into two lists is scored once per probed
    // list it sits in, so without the dedupe one top-k holds the same document in two slots
    // and displaces a real neighbour — a shorter, wrong answer that still looks like a
    // ranking.
    let store = Arc::new(MemoryStore::new());
    let key = Key::new("seg");
    let docs: Vec<Document> = (0..300).map(doc).collect();
    let built = pstore_index::vec_index::build_all(
        &docs,
        replicating(),
        pstore_format::DEFAULT_FIELD,
        None,
        None,
    );
    let centroids = built.centroids.as_ref().expect("clustered");
    let entries: usize = centroids.spans.iter().map(|(_, l)| *l as usize).sum();
    assert!(
        entries > docs.len(),
        "the fixture replicated nothing: {entries} list entries for {} documents",
        docs.len()
    );
    store.put(&key, built.segment).await.unwrap();
    store
        .put(
            &pstore_index::vec_index::centroid_key(&key),
            bytes::Bytes::from(centroids.encode()),
        )
        .await
        .unwrap();

    let idx = pstore_index::vec_index::VecIndex::open(
        store.as_ref(),
        &key,
        &pstore_index::vec_index::centroid_key(&key),
        DIM,
    )
    .await
    .unwrap();
    // Probe widely, so a replicated vector is reached through more than one of its lists.
    let hits = idx
        .search(
            store.as_ref(),
            &key,
            docs[7].vector(),
            pstore_index::vec_index::Query {
                k: 20,
                p: 16,
                ..pstore_index::vec_index::Query::default()
            },
        )
        .await
        .unwrap();

    let mut rows: Vec<usize> = hits.iter().map(|(r, _)| *r).collect();
    let before = rows.len();
    rows.sort_unstable();
    rows.dedup();
    assert_eq!(
        rows.len(),
        before,
        "a document appeared more than once in one top-k, displacing a real neighbour"
    );
    assert_eq!(before, 20, "the top-k came back short");
}
