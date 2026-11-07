//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M202: what a lost node leaves, counted. A node owns nothing, so every durable write it
//! acknowledged is in the store for any other node; only its unflushed (`batched`) rows go
//! with it.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(20_200);

fn doc(id: String) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

async fn ids<S: pstore_blob::BlobStore>(e: &Engine<S>) -> Vec<String> {
    let mut ids: Vec<String> = e
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    ids.sort();
    ids
}

fn named(range: std::ops::Range<u32>) -> Vec<String> {
    range.map(|i| format!("d{i:02}")).collect()
}

#[tokio::test]
async fn a_lost_node_loses_no_durable_write() {
    let acct = Accounted::new(MemoryStore::new());
    let store = Arc::new(acct.as_tenant(T));
    {
        let a = Engine::new(Arc::clone(&store), T, LaneId(1));
        // 10 durable writes, folded.
        for id in named(0..10) {
            a.write("idx", vec![doc(id)]).await.unwrap();
            a.flush().await.unwrap();
        }
        a.fold().await.unwrap();
        // One more durable write, not folded.
        a.write("idx", vec![doc("d10".to_owned())]).await.unwrap();
        a.flush().await.unwrap();
        // 5 batched writes, never flushed.
        for id in named(11..16) {
            a.write("idx", vec![doc(id)]).await.unwrap();
        }
        // Lost: no shutdown, no flush, no fold.
    }

    let b = Engine::new(Arc::clone(&store), T, LaneId(2));
    let lists = acct.count(T, OpClass::List);
    let (reads, writes) = (acct.count(T, OpClass::Read), acct.count(T, OpClass::Write));
    // Before a fold, B sees what was folded: the unfolded durable write waits for one.
    assert_eq!(ids(&b).await, named(0..10), "before the next fold");
    b.fold().await.unwrap();
    // Every durable write; none of the 5 that were never flushed (the batched RPO).
    assert_eq!(ids(&b).await, named(0..11), "after the next fold");
    assert_eq!(
        acct.count(T, OpClass::List),
        lists,
        "taking over a lost node's tenant listed"
    );
    println!(
        "M202 node loss: B's two scans and its fold cost {} reads and {} writes",
        acct.count(T, OpClass::Read) - reads,
        acct.count(T, OpClass::Write) - writes,
    );
}
