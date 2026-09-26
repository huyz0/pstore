//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Upsert and delete in the engine — M9c.2: delete vectors through compaction, GC, `scan`,
//! and a clustered segment that is mostly superseded.

use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::{Document, Value};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

fn doc(id: &str, v: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, v as f32 / 100.0]);
    d.attrs.insert("v".to_owned(), Value::Int(v));
    d
}

/// Every `(id, v)` a scan returns, sorted.
async fn scanned(e: &Engine<MemoryStore>) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = e
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| {
            let v = match d.attrs.get("v") {
                Some(Value::Int(v)) => *v,
                other => panic!("{other:?}"),
            };
            (d.id, v)
        })
        .collect();
    out.sort();
    out
}

fn pairs(p: &[(&str, i64)]) -> Vec<(String, i64)> {
    p.iter().map(|(i, v)| ((*i).to_owned(), *v)).collect()
}

async fn folded(e: &Engine<MemoryStore>, docs: Vec<Document>) {
    e.write("idx", docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

#[tokio::test]
async fn a_scan_sees_only_each_ids_newest_version() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(1), LaneId(1));
    folded(&e, vec![doc("a", 1), doc("b", 1), doc("c", 1)]).await;
    // Unfolded operations shadow folded rows.
    e.write("idx", vec![doc("a", 2)]).await.unwrap();
    e.delete("idx", vec!["b".to_owned()]).await.unwrap();
    assert_eq!(scanned(&e).await, pairs(&[("a", 2), ("c", 1)]));
    // Folded, the delete vector does it.
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(scanned(&e).await, pairs(&[("a", 2), ("c", 1)]));
    // Filtered, too: the vector applies to the rows the filter selects.
    let only = e
        .scan("idx", Some(&pstore_format::Filter::Gt("v".to_owned(), 0)))
        .await
        .unwrap();
    let ids: BTreeSet<String> = only.into_iter().map(|d| d.id).collect();
    assert_eq!(ids, BTreeSet::from(["a".to_owned(), "c".to_owned()]));
}

#[tokio::test]
async fn compaction_drops_deleted_rows_and_buries_their_vectors() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(2), LaneId(1));
    folded(&e, vec![doc("a", 1), doc("b", 1)]).await;
    folded(&e, vec![doc("c", 1)]).await;
    e.delete("idx", vec!["a".to_owned()]).await.unwrap();
    e.write("idx", vec![doc("c", 2)]).await.unwrap();
    e.flush().await.unwrap();
    let before = e.fold().await.unwrap();
    assert_eq!(
        e.head_for_test().await.deletes.len(),
        2,
        "both old segments carry a vector"
    );
    e.compact("idx").await.unwrap().expect("a merge");
    let head = e.head_for_test().await;
    assert!(head.deletes.is_empty(), "{:?}", head.deletes);
    let rows: usize = head.indexes["idx"].iter().map(|r| r.rows as usize).sum();
    assert_eq!(rows, 2, "the merge kept a deleted row");
    assert_eq!(scanned(&e).await, pairs(&[("b", 1), ("c", 2)]));
    // The epoch before the merge still reads its own vectors.
    let legs = [Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5],
        limit: 10,
        tune: Query::default(),
    }];
    let then = e
        .query_as_of("idx", before, &legs, Fusion::Rrf { k: 60.0 }, 10)
        .await
        .unwrap();
    let mut got: Vec<String> = e.resolve(&then).into_iter().map(|(id, _)| id).collect();
    got.sort();
    assert_eq!(got, ["b", "c"]);
    // And the buried vectors are reaped with their segments, never while live.
    e.gc(0).await.unwrap();
    assert_eq!(scanned(&e).await, pairs(&[("b", 1), ("c", 2)]));
}

#[tokio::test]
async fn a_fold_racing_a_compaction_resurrects_nothing() {
    // Review B1: the merge is sealed from the inputs as they were; a fold then deletes a row of
    // an input. Committing the merge would bring the row back and bury the vector saying so.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(3), LaneId(1));
    let other = Engine::new(Arc::clone(&store), TenantId(3), LaneId(2));
    folded(&e, vec![doc("a", 1), doc("b", 1)]).await;
    folded(&e, vec![doc("c", 1)]).await;
    let merged = e
        .compact_with_interference_for_test("idx", async {
            other.delete("idx", vec!["a".to_owned()]).await.unwrap();
            other.flush().await.unwrap();
            other.fold().await.unwrap();
        })
        .await
        .unwrap();
    assert_eq!(merged, None, "a merge of stale inputs was committed");
    assert_eq!(scanned(&e).await, pairs(&[("b", 1), ("c", 1)]));
    // The next attempt merges what is current.
    e.compact("idx").await.unwrap().expect("a merge");
    assert_eq!(scanned(&e).await, pairs(&[("b", 1), ("c", 1)]));
}

#[tokio::test]
async fn gc_never_reaps_a_live_vector() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(4), LaneId(1));
    folded(&e, vec![doc("a", 1), doc("b", 1), doc("c", 1)]).await;
    e.delete("idx", vec!["a".to_owned()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // A second delete replaces the vector; the first is buried.
    e.delete("idx", vec!["b".to_owned()]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let live = e.head_for_test().await.deletes;
    assert_eq!(live.values().map(|(_, n)| *n).collect::<Vec<_>>(), [2]);
    // ⚠️ The state the commit protocol forbids -- a live vector in the graveyard -- built by
    // hand, because the guard that survives it cannot otherwise be reached.
    let (key, _) = live.values().next().unwrap().clone();
    e.commit_head_for_test(|h| h.graveyard.entry(0).or_default().push(key))
        .await
        .unwrap();
    e.gc(0).await.unwrap();
    assert_eq!(scanned(&e).await, pairs(&[("c", 1)]));
    // Every vector HEAD names is still there to read.
    for (key, _) in e.head_for_test().await.deletes.values() {
        pstore_blob::BlobStore::get(&*store, &pstore_blob::Key::new(key.clone()))
            .await
            .expect("a live vector was reaped");
    }
}

#[tokio::test]
async fn a_mostly_superseded_clustered_segment_still_answers_top_k() {
    // 600 rows in a clustered segment, the 540 nearest the query deleted. The lists a plain
    // probe reads are then nearly all deleted rows: without widening `p` by rows / live rows
    // the answer comes back short.
    const ROWS: usize = 600;
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let docs: Vec<Document> = (0..ROWS)
        .map(|i| Document::new(format!("d{i:04}"), (0..8).map(|_| next()).collect()))
        .collect();
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(5), LaneId(1)).with_index_params(
        pstore_index::cluster::Params {
            target_list_size: 40,
            exact_scan_threshold: 100,
            ..pstore_index::cluster::Params::default()
        },
    );
    folded(&e, docs.clone()).await;
    let q = docs[0].vector().to_vec();
    let dot = |d: &Document| d.vector().iter().zip(&q).map(|(a, b)| a * b).sum::<f32>();
    let mut by: Vec<&Document> = docs.iter().collect();
    by.sort_by(|a, b| dot(b).total_cmp(&dot(a)));
    let gone: Vec<String> = by[..540].iter().map(|d| d.id.clone()).collect();
    let live: BTreeSet<String> = by[540..].iter().map(|d| d.id.clone()).collect();
    e.delete("idx", gone).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let legs = [Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: q.clone(),
        limit: 10,
        tune: Query::default(),
    }];
    let answer = e
        .query("idx", &legs, Fusion::Rrf { k: 60.0 }, 10)
        .await
        .unwrap();
    let got: BTreeSet<String> = e.resolve(&answer).into_iter().map(|(id, _)| id).collect();
    assert_eq!(got.len(), 10, "{got:?}");
    assert!(got.is_subset(&live), "a deleted row was served: {got:?}");
}

/// Ten documents ranked by a text leg and by a sparse leg alike -- `d9` best -- folded, then
/// the best three deleted and folded again, so each leg's own top candidates are deleted rows.
async fn best_three_deleted() -> Engine<MemoryStore> {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(6), LaneId(1));
    let docs: Vec<Document> = (0..10)
        .map(|i| {
            let mut d = Document::new(format!("d{i}"), vec![1.0, 0.5]);
            let words: Vec<&str> = std::iter::repeat_n("zebra", i + 1)
                .chain(std::iter::repeat_n("filler", 10 - i))
                .collect();
            d.attrs.insert(
                pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
                Value::Str(words.join(" ")),
            );
            d.vectors.insert(
                "sparse".to_owned(),
                pstore_format::VectorField::Sparse(vec![(
                    1,
                    pstore_format::Impact::new(0.05 * (i + 1) as f32),
                )]),
            );
            d
        })
        .collect();
    folded(&e, docs).await;
    e.delete(
        "idx",
        vec!["d9".to_owned(), "d8".to_owned(), "d7".to_owned()],
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e
}

async fn ranked(e: &Engine<MemoryStore>, leg: Prefetch) -> Vec<String> {
    let answer = e
        .query("idx", &[leg], Fusion::Rrf { k: 60.0 }, 5)
        .await
        .unwrap();
    e.resolve(&answer).into_iter().map(|(id, _)| id).collect()
}

#[tokio::test]
async fn text_and_sparse_legs_are_widened_past_deleted_rows_too() {
    let e = best_three_deleted().await;
    let text = ranked(
        &e,
        Prefetch::Text {
            field: pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
            query: "zebra".to_owned(),
            limit: 5,
        },
    )
    .await;
    assert_eq!(text, ["d6", "d5", "d4", "d3", "d2"]);
    let sparse = ranked(
        &e,
        Prefetch::Sparse {
            field: "sparse".to_owned(),
            query: vec![(1, 1.0)],
            limit: 5,
        },
    )
    .await;
    assert_eq!(sparse, ["d6", "d5", "d4", "d3", "d2"]);
}
