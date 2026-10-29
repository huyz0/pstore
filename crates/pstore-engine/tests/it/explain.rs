//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M165: the engine's part of a query's plan -- the fresh segment named so no figure counts
//! it, and a retried attempt's plan replacing the failed one's.

use pstore_blob::MemoryStore;
use pstore_engine::{Consistency, Engine};
use pstore_format::{DEFAULT_FIELD, Document};
use pstore_query::explain::collect;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;
use std::time::Duration;

const T: TenantId = TenantId(165);

fn dense(limit: usize) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.0],
        limit,
        tune: pstore_index::vec_index::Query::default(),
    }]
}

fn docs(ids: &[&str]) -> Vec<Document> {
    ids.iter()
        .map(|id| Document::new((*id).to_owned(), vec![1.0, 0.5]))
        .collect()
}

#[tokio::test]
async fn explain_counts_shadow_and_excludes_the_fresh_segment() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    e.write("idx", docs(&["d0", "d1", "d2"])).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // Unfolded here: an overwrite of a folded id, and a new one.
    e.write("idx", docs(&["d0", "d9"])).await.unwrap();
    e.flush().await.unwrap();
    let (got, x) = collect(e.query("idx", &dense(2), Fusion::Rrf { k: 60.0 }, 2)).await;
    assert!(!got.unwrap().unfolded.is_empty(), "no fresh segment ran");
    assert_eq!((x.segments, x.shadow), (1, 2), "{x:?}");
    // The one folded segment, asked for its limit and the two shadowed ids; never the fresh
    // segment's own ask.
    let leg = &x.legs[0];
    assert!(leg.widened, "{leg:?}");
    assert_eq!(leg.requested, 2 + 2, "{leg:?}");
    assert_eq!(leg.exact_segments + leg.approximate_segments, 1, "{leg:?}");
}

async fn bounded(
    e: &Engine<MemoryStore>,
) -> Result<pstore_engine::Answer, pstore_engine::EngineError> {
    let level = Consistency::Bounded(Duration::from_secs(60));
    e.query_filtered_as("idx", &dense(3), None, Fusion::Rrf { k: 60.0 }, 3, level)
        .await
}

#[tokio::test]
async fn explain_keeps_only_the_last_attempt() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    for batch in [["a", "b"], ["c", "d"]] {
        e.write("idx", docs(&batch)).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    // Cached: a HEAD naming two segments, which a compaction and GC then reap.
    let (_, cached) = collect(bounded(&e)).await;
    assert_eq!(cached.segments, 2);
    let w = Engine::new(Arc::clone(&store), T, LaneId(2));
    w.compact("idx").await.unwrap();
    assert!(w.gc(0).await.unwrap() > 0, "nothing reaped");
    // The cached attempt fails on a reaped segment and the fresh one answers: its plan alone.
    let (got, x) = collect(bounded(&e)).await;
    assert_eq!(got.unwrap().hits.len(), 3);
    assert_eq!(x.segments, 1, "{x:?}");
    assert_eq!(x.legs.len(), 1, "{x:?}");
    assert_eq!(x.legs[0].requested, 3, "{x:?}");
}
