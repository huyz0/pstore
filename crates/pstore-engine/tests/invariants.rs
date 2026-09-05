//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! D-34: the round-trip budget is a **tested invariant**, not a design claim.
//!
//! ~30 ms per hop against a ~100 ms budget allows about three sequential fetches. Every
//! added hop is a third of the budget, and no functional test can see one — the answer is
//! identical either way, only slower.
use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::{Document, Filter, Value};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![n as f32, 1.0]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

async fn seeded(
    rows: i64,
    folds: usize,
) -> (
    Arc<DepthCounting<MemoryStore>>,
    Engine<DepthCounting<MemoryStore>>,
) {
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let e = Engine::new(Arc::clone(&s), TenantId(1), LaneId(1));
    let per = rows / folds as i64;
    for f in 0..folds as i64 {
        let batch: Vec<_> = (f * per..(f + 1) * per)
            .map(|i| doc(&format!("d{i}"), i))
            .collect();
        e.write("idx", batch).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    (s, e)
}

#[tokio::test]
async fn a_cold_read_has_a_sequential_depth_of_at_most_three() {
    // HEAD, then the segment's footer, then its blocks. Three.
    let (s, e) = seeded(500, 1).await;
    s.reset();
    let got = e.scan("idx", None).await.unwrap();
    assert_eq!(got.len(), 500);
    assert!(
        s.depth() <= 3,
        "cold scan took {} sequential round trips",
        s.depth()
    );
}

#[tokio::test]
async fn a_cold_filtered_read_has_a_sequential_depth_of_at_most_three() {
    let (s, e) = seeded(500, 1).await;
    s.reset();
    let got = e
        .scan("idx", Some(&Filter::Gt("n".to_owned(), 480)))
        .await
        .unwrap();
    assert_eq!(got.len(), 19);
    assert!(
        s.depth() <= 3,
        "filtered scan took {} round trips",
        s.depth()
    );
}

#[tokio::test]
async fn a_cold_search_has_a_sequential_depth_of_at_most_three() {
    let (s, e) = seeded(500, 1).await;
    s.reset();
    let hits = e.search("idx", &[250.0, 1.0], 5, None).await.unwrap();
    assert_eq!(hits[0].0, "d250");
    assert!(s.depth() <= 3, "search took {} round trips", s.depth());
}

#[tokio::test]
async fn depth_does_not_grow_with_the_number_of_segments() {
    // The mutation this catches is a `for` loop over segment refs. It is invisible
    // functionally and turns a ten-segment index into a twenty-hop query -- 600 ms of
    // pure latency, which is the entire budget six times over.
    let (s, e) = seeded(600, 10).await;
    s.reset();
    let got = e.scan("idx", None).await.unwrap();
    assert_eq!(got.len(), 600);
    assert!(
        s.depth() <= 3,
        "ten segments cost {} sequential round trips; segments must be opened together",
        s.depth()
    );
}

#[tokio::test]
async fn a_write_batch_is_one_round_trip_and_one_request() {
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let e = Engine::new(Arc::clone(&s), TenantId(2), LaneId(1));
    // Warm the lane first: registering it is one CAS per lane LIFETIME, so folding it
    // into the per-batch invariant would misstate the steady-state cost this bounds.
    e.write("idx", vec![doc("warm", 0)]).await.unwrap();
    e.flush().await.unwrap();

    e.write("idx", (0..1000).map(|i| doc(&format!("d{i}"), i)).collect())
        .await
        .unwrap();
    s.reset();
    e.flush().await.unwrap();
    assert_eq!(s.requests(), 1, "a batch of a thousand is one request");
    assert_eq!(s.depth(), 1);
}

#[tokio::test]
async fn a_fresh_read_costs_no_blob_requests_beyond_head() {
    // The freshness layer: unfolded rows are served from memory, so seeing a just-written
    // document does not depend on the blob store at all beyond checking the epoch.
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let e = Engine::new(Arc::clone(&s), TenantId(3), LaneId(1));
    e.write("idx", vec![doc("fresh", 1)]).await.unwrap();
    s.reset();
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 1);
    assert!(
        s.depth() <= 1,
        "a fresh read took {} round trips",
        s.depth()
    );
}

#[tokio::test]
async fn nothing_on_the_read_path_ever_lists() {
    // Design rule 4. A LIST is priced like a PUT, returns at most 1000 keys, and is
    // inherently serial -- and every key in this design is derived, so nothing needs one.
    use pstore_blob::{Accounted, OpClass};
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(4);
    let e = Engine::new(Arc::new(s.as_tenant(t)), t, LaneId(1));
    e.write("idx", (0..100).map(|i| doc(&format!("d{i}"), i)).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.scan("idx", None).await.unwrap();
    e.search("idx", &[1.0, 1.0], 3, None).await.unwrap();
    assert_eq!(s.count(t, OpClass::List), 0, "something listed");
}
