//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `Engine::query` — M5g.2, the indexed read path from a tenant id.
//!
//! ⚠️ **It is indexed and stale; `Engine::search` is fresh and exact.** A caller must choose,
//! and neither name says so. The gap is the memtable's: unfolded rows live in memory with no
//! index, no segment and no row ordinal, so they cannot enter a `(segment, row)` fusion.
//! `an_unfolded_row_is_visible_to_scan_and_not_to_query` pins the limit so it is stated rather
//! than discovered.

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::{DEFAULT_FIELD, Document};
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const DIM: usize = 8;

/// ⚠️ Component 0 rises with `i` and dominates the rest, so a dot-product ranking orders
/// documents by index. Without it every batch holds the same vectors — the first fixture used
/// `(i + j) % 13`, which repeats every 13 documents, so segment 0 tied with segment 1 on every
/// query and the tie-break `(segment, row)` hid whether the second segment was read at all.
fn doc(i: usize) -> Document {
    Document::new(
        format!("d{i:05}"),
        (0..DIM)
            .map(|j| {
                if j == 0 {
                    i as f32 / 100.0
                } else {
                    ((i + j) % 13) as f32 / 13.0
                }
            })
            .collect(),
    )
}

fn params() -> pstore_index::cluster::Params {
    pstore_index::cluster::Params {
        target_list_size: 40,
        exact_scan_threshold: 100,
        ..pstore_index::cluster::Params::default()
    }
}

fn dense(i: usize) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: doc(i).vector().to_vec(),
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }]
}

#[tokio::test]
async fn a_query_answers_from_head_over_every_segment() {
    let t = TenantId(710);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(t));
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params());

    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let one = e
        .query("idx", &dense(299), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(
        !one.is_empty(),
        "an indexed query over one segment found nothing"
    );
    assert!(one.iter().all(|h| h.segment == 0));

    // ⚠️ A second fold changes the answer, which is what "from HEAD" means: the query reads
    // the segment list rather than remembering one.
    e.write("idx", (300..600).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let two = e
        .query("idx", &dense(599), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(
        two.iter().any(|h| h.segment == 1),
        "the second segment contributed nothing, so only one was queried: {two:?}"
    );
    assert_eq!(acct.count(t, OpClass::List), 0, "an indexed query listed");
}

#[tokio::test]
async fn an_unfolded_row_is_visible_to_scan_and_not_to_query() {
    // ⚠️ The stated limit, pinned in both directions so it cannot change quietly. Freshness is
    // `scan`'s and `search`'s; the index is `query`'s.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(711);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params());
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    // Written and not folded: it is durable, and it is in no segment.
    e.write("idx", vec![doc(9_999)]).await.unwrap();
    e.flush().await.unwrap();

    assert!(
        e.scan("idx", None)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == "d09999"),
        "the freshness layer stopped working"
    );
    let hits = e
        .query("idx", &dense(9_999), Fusion::default(), 10)
        .await
        .unwrap();
    let rows = e.scan("idx", None).await.unwrap();
    assert!(
        !hits.is_empty(),
        "the indexed query found nothing at all, so it cannot say anything about freshness"
    );
    assert!(
        !hits
            .iter()
            .any(|h| rows.get(h.row).is_some_and(|d| d.id == "d09999")),
        "an unfolded row reached the indexed query, which cannot be right -- it has no segment"
    );

    // And after a fold it is there.
    e.fold().await.unwrap();
    let after = e
        .query("idx", &dense(9_999), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(
        after.iter().any(|h| h.segment == 1),
        "the folded row still did not reach the query"
    );
}

#[tokio::test]
async fn a_query_probes_rather_than_scanning() {
    // ⚠️ **The centroid key's correctness is invisible in the results.** Point it at an object
    // that does not exist and the query still answers, and answers *exactly* — because a
    // missing centroid table is how an index below the exact-scan threshold says "scan me"
    // (D-10). A mutation that broke the key survived every result assertion in this file.
    // What changes is the BYTES: an exact scan reads the whole vectors section.
    let t = TenantId(712);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(t));
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params());
    e.write("idx", (0..600).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let before = acct.bytes(t, OpClass::Read);
    let probed = e
        .query("idx", &dense(599), Fusion::default(), 10)
        .await
        .unwrap();
    let probed_bytes = acct.bytes(t, OpClass::Read) - before;
    assert!(!probed.is_empty());

    // The same query with the centroid table gone: same answers, whole-section read.
    let head = e.head_for_test().await;
    for r in &head.indexes["idx"] {
        let seg = pstore_blob::Key::new(r.key.clone());
        store
            .delete_batch(&[pstore_index::vec_index::centroid_key(&seg)])
            .await
            .unwrap();
    }
    let before = acct.bytes(t, OpClass::Read);
    let scanned = e
        .query("idx", &dense(599), Fusion::default(), 10)
        .await
        .unwrap();
    let scanned_bytes = acct.bytes(t, OpClass::Read) - before;

    assert_eq!(
        probed.iter().map(|h| h.row).collect::<Vec<_>>(),
        scanned.iter().map(|h| h.row).collect::<Vec<_>>(),
        "the fixture's probe and scan disagree, so the byte comparison is not like for like"
    );
    assert!(
        probed_bytes < scanned_bytes,
        "the indexed query moved {probed_bytes} bytes against {scanned_bytes} for an exact \
         scan -- the centroid table is not being reached"
    );
}
