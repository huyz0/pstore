//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Order by attribute in the engine — M9e: paging across a compaction, and the depth.

use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::{Document, Value};
use pstore_query::{Op, OrderBy, Predicate};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(id: &str, k: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("k".to_owned(), Value::Int(k));
    d
}

fn by_id() -> OrderBy {
    OrderBy {
        attr: "id".to_owned(),
        desc: false,
    }
}

async fn page<S: pstore_blob::BlobStore>(
    e: &Engine<S>,
    after: Option<&str>,
    limit: usize,
) -> Vec<Document> {
    let filter = after.map(|c| Predicate::Cmp("id".to_owned(), Op::Gt, Value::Str(c.to_owned())));
    e.ordered("idx", &by_id(), filter.as_ref(), 0, limit, None)
        .await
        .unwrap()
        .rows
}

#[tokio::test]
async fn paging_by_id_survives_a_compaction_between_pages() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(40), LaneId(1));
    for chunk in (0..30).collect::<Vec<i64>>().chunks(10) {
        e.write(
            "idx",
            chunk.iter().map(|i| doc(&format!("d{i:02}"), *i)).collect(),
        )
        .await
        .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let mut seen = Vec::new();
    let mut d27 = None;
    let mut cursor: Option<String> = None;
    for n in 0.. {
        let rows = page(&e, cursor.as_deref(), 7).await;
        if rows.is_empty() {
            break;
        }
        // Bounded: a filter that stopped applying would return the first page forever.
        assert!(n <= 20, "the cursor stopped advancing");
        cursor = rows.last().map(|d| d.id.clone());
        d27 = d27.or_else(|| {
            rows.iter()
                .find(|d| d.id == "d27")
                .map(|d| d.attrs["k"].clone())
        });
        seen.extend(rows.into_iter().map(|d| d.id));
        if n == 1 {
            // Three segments merged into one, with a delete, an upsert and a new id beyond the
            // cursor.
            e.delete("idx", vec!["d25".to_owned()]).await.unwrap();
            e.write("idx", vec![doc("d27", -27), doc("d99", 99)])
                .await
                .unwrap();
            e.flush().await.unwrap();
            e.fold().await.unwrap();
            e.compact("idx").await.unwrap().expect("a merge");
        }
    }
    let mut want: Vec<String> = (0..30)
        .filter(|i| *i != 25)
        .map(|i| format!("d{i:02}"))
        .collect();
    want.push("d99".to_owned());
    assert_eq!(seen, want);
    assert_eq!(d27, Some(Value::Int(-27)), "the upserted version");
}

#[tokio::test]
async fn an_ordered_query_is_three_round_trips_however_many_segments() {
    for segments in [1usize, 8] {
        let store = Arc::new(DepthCounting::new(MemoryStore::new()));
        let e = Engine::new(Arc::clone(&store), TenantId(41), LaneId(1));
        for s in 0..segments {
            e.write(
                "idx",
                (0..5).map(|i| doc(&format!("s{s}-{i}"), i)).collect(),
            )
            .await
            .unwrap();
            e.flush().await.unwrap();
            e.fold().await.unwrap();
        }
        // A delete vector on one segment.
        e.delete("idx", vec!["s0-1".to_owned()]).await.unwrap();
        e.flush().await.unwrap();
        let before = e.fold().await.unwrap();
        let by = OrderBy {
            attr: "k".to_owned(),
            desc: true,
        };
        for unfolded in [false, true] {
            if unfolded {
                e.write("idx", vec![doc("late", 9)]).await.unwrap();
            }
            store.reset();
            let got = e.ordered("idx", &by, None, 0, 3, None).await.unwrap();
            assert_eq!(store.depth(), 3, "{segments} segments, unfolded {unfolded}");
            // Exactly: HEAD; each segment's suffix, and the one delete vector; one coalesced
            // block plan per segment.
            assert_eq!(
                store.requests(),
                2 + 2 * segments,
                "{segments} segments, unfolded {unfolded}"
            );
            assert_eq!(got.rows[0].id == "late", unfolded);
            assert_eq!(got.unfolded, usize::from(unfolded));
        }
        store.reset();
        e.ordered("idx", &by, None, 0, 3, Some(before))
            .await
            .unwrap();
        assert_eq!(store.depth(), 3, "{segments} segments, as_of");
    }
}
