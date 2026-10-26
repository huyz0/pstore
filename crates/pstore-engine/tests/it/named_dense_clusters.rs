//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M83: a named dense field of a big segment is clustered, and a dense leg over it reads the
//! best lists' rows instead of the whole field.

use pstore_blob::{Accounted, Key, MemoryStore};
use pstore_engine::Engine;
use pstore_format::{Document, VectorField};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(8300);
const DOCS: usize = 9_000;

/// A document in one of 12 topics: `tag` is 4-dimensional and topical, `vector` points nowhere
/// near it. `lacking` leaves `tag` off every seventh document.
fn doc(i: usize, lacking: bool) -> Document {
    let topic = i % 12;
    let mut s = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut r = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 2000) as f32 / 1000.0 - 1.0
    };
    let base = |t: usize, d: usize| ((t * 7 + d * 3) % 11) as f32 - 5.0;
    let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.5, 0.25, 0.125]);
    if !(lacking && i.is_multiple_of(7)) {
        let v: Vec<f32> = (0..4).map(|k| base(topic, k) + 0.3 * r()).collect();
        // And a field of two vectors a document, which is `multi`'s, never a dense field's.
        let w: Vec<f32> = v.iter().map(|x| -x).collect();
        d.vectors
            .insert("pair".into(), VectorField::Dense(vec![v.clone(), w]));
        d.vectors.insert("tag".into(), VectorField::Dense(vec![v]));
    }
    d
}

async fn engine(
    lacking: bool,
) -> (
    Engine<pstore_blob::TenantView<MemoryStore>>,
    Arc<Accounted<MemoryStore>>,
    Arc<pstore_blob::TenantView<MemoryStore>>,
) {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("big", (0..DOCS).map(|i| doc(i, lacking)).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    (e, acct, store)
}

fn leg(p: usize, exact: bool) -> Prefetch {
    Prefetch::Dense {
        field: "tag".to_owned(),
        query: vec![-2.0, 1.0, 4.0, 0.0],
        limit: 10,
        tune: Query {
            p,
            exact,
            ..Query::default()
        },
    }
}

async fn fdev_bytes(
    e: &Engine<pstore_blob::TenantView<MemoryStore>>,
    acct: &Accounted<MemoryStore>,
    p: usize,
    exact: bool,
) -> (u64, Vec<(String, u32)>) {
    acct.record_ranges();
    let a = e
        .query("big", &[leg(p, exact)], Fusion::default(), 10)
        .await
        .unwrap();
    let moved = acct
        .ranges()
        .iter()
        .filter(|(k, _)| k.as_str().ends_with(".fdev"))
        .map(|(_, r)| r.end - r.start)
        .sum();
    let ranked = a
        .ids
        .iter()
        .zip(&a.hits)
        .map(|(id, h)| (id.clone().unwrap(), h.score.to_bits()))
        .collect();
    (moved, ranked)
}

#[tokio::test]
async fn a_clustered_named_field_reads_a_fraction_of_its_bytes() {
    let (e, acct, store) = engine(false).await;
    let head = e.head_for_test().await;
    let seg = Key::new(head.indexes["big"][0].key.clone());
    let set = pstore_index::fde_clusters::FdeSet::open(&*store, &seg)
        .await
        .unwrap();
    assert!(
        set.field(&pstore_index::fde_clusters::dense_name("tag"))
            .is_some(),
        "the segment clustered `tag`"
    );
    assert!(
        set.field(&pstore_index::fde_clusters::dense_name("pair"))
            .is_none(),
        "a field of two vectors a document is not a dense field"
    );
    let (one, _) = fdev_bytes(&e, &acct, 1, false).await;
    let (all, all_ranked) = fdev_bytes(&e, &acct, 1 << 20, false).await;
    let (exact_bytes, exact_ranked) = fdev_bytes(&e, &acct, 8, true).await;
    eprintln!("p=1: {one}  all lists: {all}");
    // 9,000 four-dimensional rows are 144,000 bytes and make a few lists of ~4,000 rows
    // (`target_list_size`): one list is a fraction of the field, every list is all of it.
    assert_eq!(all, (DOCS * 16) as u64, "every list is the whole field");
    assert!(one > 0 && one * 2 <= all + 16 * 4_000, "{one} of {all}");
    // The exact scan reads the field from the segment, not the sidecar.
    assert_eq!(exact_bytes, 0);
    // At every list the clustered leg ranks and scores exactly as the exact scan does.
    assert_eq!(all_ranked, exact_ranked);
}

#[tokio::test]
async fn recall_of_a_named_field_at_one_probe() {
    let (e, acct, _) = engine(false).await;
    // 12 topics, queried at one point of each, against the exact scan.
    let mut hit = 0;
    let mut want = 0;
    for t in 0..12 {
        let base = |d: usize| ((t * 7 + d * 3) % 11) as f32 - 5.0;
        let q: Vec<f32> = (0..4).map(|d| base(d) + 0.1).collect();
        let run = |exact: bool| {
            // One probe of the segment's lists: the hardest setting the leg allows.
            let leg = Prefetch::Dense {
                field: "tag".to_owned(),
                query: q.clone(),
                limit: 10,
                tune: Query {
                    exact,
                    p: 1,
                    ..Query::default()
                },
            };
            let e = &e;
            async move {
                e.query("big", &[leg], Fusion::default(), 10)
                    .await
                    .unwrap()
                    .ids
            }
        };
        let (approx, exact) = (run(false).await, run(true).await);
        want += exact.len();
        hit += exact.iter().filter(|id| approx.contains(id)).count();
    }
    let recall = hit as f64 / want as f64;
    eprintln!("recall@10 at one probe: {recall:.4}");
    assert!(recall >= 0.9, "{recall}");
    let _ = acct;
}

#[tokio::test]
async fn a_field_some_row_lacks_keeps_the_exact_scan() {
    let (e, acct, store) = engine(true).await;
    let head = e.head_for_test().await;
    let seg = Key::new(head.indexes["big"][0].key.clone());
    // Clustering it would score the missing rows as zero vectors; the exact scan skips them.
    let has = match pstore_index::fde_clusters::FdeSet::open(&*store, &seg).await {
        Ok(set) => set
            .field(&pstore_index::fde_clusters::dense_name("tag"))
            .is_some(),
        Err(_) => false,
    };
    assert!(!has, "a field with missing rows is not clustered");
    let (moved, ranked) = fdev_bytes(&e, &acct, 8, false).await;
    assert_eq!(moved, 0, "nothing came from a sidecar");
    assert_eq!(ranked.len(), 10);
    assert!(
        ranked.iter().all(|(id, _)| {
            let i: usize = id[1..].parse().unwrap();
            !i.is_multiple_of(7)
        }),
        "no row that lacks the field is ranked"
    );
}

#[tokio::test]
async fn a_compaction_rebuilds_the_clusters() {
    let (e, _acct, store) = engine(false).await;
    // A second segment, then merge both.
    e.write("big", (DOCS..DOCS + 3_000).map(|i| doc(i, false)).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert!(e.compact("big").await.unwrap().is_some());
    let head = e.head_for_test().await;
    let segs = &head.indexes["big"];
    assert_eq!(segs.len(), 1, "merged");
    let seg = Key::new(segs[0].key.clone());
    let set = pstore_index::fde_clusters::FdeSet::open(&*store, &seg)
        .await
        .unwrap();
    assert!(
        set.field(&pstore_index::fde_clusters::dense_name("tag"))
            .is_some()
    );
}

#[tokio::test]
async fn a_nul_in_a_field_name_is_refused() {
    let store = Arc::new(Accounted::new(MemoryStore::new()).as_tenant(T));
    let e = Engine::new(store, T, LaneId(1));
    let mut d = Document::new("a".to_owned(), vec![1.0]);
    d.vectors
        .insert("a\0b".into(), VectorField::Dense(vec![vec![1.0]]));
    let err = e.write("x", vec![d]).await.unwrap_err().to_string();
    assert!(err.contains("NUL"), "{err}");
}
