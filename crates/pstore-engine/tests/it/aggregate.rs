//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A count at a past epoch is exact after that epoch's segments and delete vectors are buried
//! (M12, code review B1): `Head::as_of` rebuilds buried objects with a count of zero, so only
//! the full path, which reads them, can answer it.

use pstore_blob::MemoryStore;
use pstore_engine::{Consistency, Engine};
use pstore_format::Document;
use pstore_query::{Aggregate, AggregateSpec, Total};
use pstore_types::{Epoch, LaneId, TenantId};
use std::sync::Arc;

fn count() -> AggregateSpec {
    AggregateSpec {
        labels: vec![("n".to_owned(), Aggregate::Count(None))],
        group_by: vec![],
        top_k: 10,
    }
}

async fn n(e: &Engine<MemoryStore>, as_of: Option<Epoch>) -> Total {
    let got = e
        .aggregate_as("idx", count(), None, as_of, Consistency::Eventual)
        .await
        .unwrap();
    got.groups[0].1[0]
}

async fn four_segments(e: &Engine<MemoryStore>) -> Epoch {
    for k in 0..4 {
        let docs = (k * 500..(k + 1) * 500)
            .map(|i| Document::new(format!("d{i:05}"), vec![1.0, 0.5]))
            .collect();
        e.write("idx", docs).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.epoch()
}

#[tokio::test]
async fn a_past_count_survives_a_compaction() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(40), LaneId(1));
    let then = four_segments(&e).await;
    assert!(
        e.compact("idx").await.unwrap().is_some(),
        "nothing compacted"
    );
    assert_eq!(n(&e, None).await, Total::Int(2000));
    assert_eq!(n(&e, Some(then)).await, Total::Int(2000));
}

#[tokio::test]
async fn a_past_count_survives_a_replaced_delete_vector() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(41), LaneId(1));
    four_segments(&e).await;
    e.delete("idx", vec!["d00001".to_owned()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let one_gone = e.epoch();
    // A second delete in the same segment replaces its vector, burying the first.
    e.delete("idx", vec!["d00002".to_owned()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(n(&e, None).await, Total::Int(1998));
    assert_eq!(n(&e, Some(one_gone)).await, Total::Int(1999));
}
