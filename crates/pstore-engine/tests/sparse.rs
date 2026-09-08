//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M5a.2 — a sparse field survives the whole write path.
//!
//! ⚠️ **The failure this file exists to catch is silent.** Every stage between `write` and a
//! merged segment reconstructs a document from something narrower than the document: the
//! bundle carried one dense vector, a segment block carries no vector at all, and a
//! compaction merges whatever `scan` handed back. A field that any one of them drops comes
//! back as *absent*, which is indistinguishable from never having been written.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::{DEFAULT_FIELD, Document, Impact, VectorField, sparse};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const FIELD: &str = "body_sparse";

fn hybrid(i: usize) -> Document {
    let mut d = Document::new(format!("d{i:04}"), vec![i as f32, 1.0, 0.5]);
    d.vectors.insert(
        FIELD.to_owned(),
        VectorField::Sparse(vec![
            (i as u32 % 17, Impact::new(0.75)),
            (100 + (i as u32 % 3), Impact::new(0.25)),
        ]),
    );
    d
}

fn sparse_of(d: &Document) -> Vec<(u32, f32)> {
    match d.vectors.get(FIELD) {
        Some(VectorField::Sparse(p)) => p.iter().map(|(x, w)| (*x, w.get())).collect(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn a_sparse_field_survives_write_fold_and_compaction() {
    // ⚠️ Criterion 6. Three narrowings in a row, each of which loses the field silently:
    // the bundle (one dense vector until M5a.2), the segment block (no vectors at all), and
    // the compaction (merges what `scan` returned). Asserted after each, so a failure names
    // the stage rather than the outcome.
    let store = Accounted::new(MemoryStore::new());
    let t = TenantId(500);
    let e = Engine::new(Arc::new(store.as_tenant(t)), t, LaneId(1));

    let want: Vec<Document> = (0..40).map(hybrid).collect();
    for batch in want.chunks(20) {
        e.write("hybrid", batch.to_vec()).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }

    let folded = e.scan("hybrid", None).await.unwrap();
    assert_eq!(folded.len(), 40, "rows were lost before compaction");
    check(&folded, &want, "after the fold");

    e.compact("hybrid").await.unwrap();
    let merged = e.scan("hybrid", None).await.unwrap();
    assert_eq!(merged.len(), 40, "rows were lost in the merge");
    check(&merged, &want, "after the compaction");

    assert_eq!(store.count(t, OpClass::List), 0, "the sparse path listed");
}

fn check(got: &[Document], want: &[Document], when: &str) {
    for w in want {
        let g = got
            .iter()
            .find(|g| g.id == w.id)
            .unwrap_or_else(|| panic!("{when}: {} is missing entirely", w.id));
        let (gs, ws) = (sparse_of(g), sparse_of(w));
        assert!(
            !gs.is_empty(),
            "{when}: {} came back with NO sparse field -- which reads exactly like never \
             having written one",
            w.id
        );
        assert_eq!(
            gs.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
            ws.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
            "{when}: {}'s dimensions changed",
            w.id
        );
        for ((_, a), (_, b)) in gs.iter().zip(&ws) {
            assert!(
                (a - b).abs() <= b.abs() / 254.0 + 1e-6,
                "{when}: impact {a} vs {b}"
            );
        }
        assert_eq!(
            g.field(DEFAULT_FIELD),
            w.field(DEFAULT_FIELD),
            "{when}: {}'s dense field was disturbed by the sparse one",
            w.id
        );
    }
}

#[tokio::test]
async fn a_folded_segment_has_its_dictionary_beside_it() {
    // The sidecar is addressed by derivation, so a compaction can reach the dictionary of
    // every input it merges without a LIST. If the fold does not write it there, the merge
    // reads nothing and loses the field -- with the postings still in the segment, intact
    // and unreachable.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(501);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    e.write("hybrid", (0..8).map(hybrid).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let head = e.head_for_test().await;
    let refs = head.indexes.get("hybrid").expect("no segments");
    assert!(!refs.is_empty());
    for r in refs {
        let seg = Key::new(&r.key);
        let dict = sparse::dict_key(&seg);
        let raw = store
            .get(&dict)
            .await
            .unwrap_or_else(|_| panic!("no dictionary beside {}", seg.as_str()));
        assert!(
            sparse::Dictionary::decode(&raw).is_some(),
            "the sidecar is not a dictionary"
        );
    }
}

#[tokio::test]
async fn a_dense_only_index_writes_no_sidecar_and_costs_nothing_extra() {
    // ⚠️ The regression the sparse path could cause everywhere else. A dictionary written
    // for every segment would add a PUT per fold to indexes that have no sparse field at
    // all, and the write budget is 1 PUT per segment.
    let store = Accounted::new(MemoryStore::new());
    let t = TenantId(502);
    let e = Engine::new(Arc::new(store.as_tenant(t)), t, LaneId(1));
    let docs: Vec<Document> = (0..8)
        .map(|i| Document::new(format!("d{i}"), vec![i as f32, 1.0]))
        .collect();
    e.write("dense", docs).await.unwrap();
    e.flush().await.unwrap();
    let before = store.count(t, OpClass::Write);
    e.fold().await.unwrap();
    let spent = store.count(t, OpClass::Write) - before;
    assert!(
        spent <= 2,
        "a dense fold spent {spent} writes: one segment and one HEAD is the budget, and a \
         sidecar was written for an index with no sparse field"
    );
}

#[tokio::test]
async fn a_reaped_sparse_segment_takes_its_dictionary_with_it() {
    // ⚠️ A sidecar is reachable ONLY by derivation from its segment. Reap the segment and
    // leave the dictionary, and nothing in the system can ever name that object again — it
    // is not in the graveyard, not in HEAD, and not findable without the LIST this design
    // does not have.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(503);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    for batch in 0..2 {
        e.write("hybrid", (batch * 8..batch * 8 + 8).map(hybrid).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let before: Vec<Key> = e
        .head_for_test()
        .await
        .indexes
        .get("hybrid")
        .unwrap()
        .iter()
        .map(|r| Key::new(&r.key))
        .collect();
    assert_eq!(before.len(), 2, "the fixture needs two segments to merge");

    e.compact("hybrid").await.unwrap();
    // Enough epochs to pass the retention window, then reap.
    for _ in 0..3 {
        e.write("other", vec![Document::new("x", vec![1.0])])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.gc(1).await.unwrap();

    for seg in &before {
        assert!(
            store.get(seg).await.is_err(),
            "the merged-away segment {} survived gc",
            seg.as_str()
        );
        assert!(
            store.get(&sparse::dict_key(seg)).await.is_err(),
            "the dictionary of {} outlived it, and nothing can name it now",
            seg.as_str()
        );
    }
}
