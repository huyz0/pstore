//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M6c.3 — the engine indexes the attribute it is told, and carries it through a merge.
//!
//! ⚠️ **The failure here is silent in both directions.** A corpus whose prose is in `body`
//! folds to a segment with no postings at all and no error, and a compaction that rebuilds
//! over the wrong attribute destroys an index that was there — with every row still present,
//! so a scan cannot tell.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::{Engine, EngineError};
use pstore_format::{Document, Section, Segment, Value, text};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(i: usize, attr: &str) -> Document {
    let mut d = Document::new(format!("d{i:04}"), vec![i as f32, 1.0, 0.5]);
    d.attrs.insert(
        attr.to_owned(),
        Value::Str(format!("row {i} quarterly revenue")),
    );
    d
}

/// Every segment HEAD names for `index`, opened.
async fn segments(store: &Arc<MemoryStore>, t: TenantId, index: &str) -> Vec<(Key, Segment)> {
    let e = Engine::new(Arc::clone(store), t, LaneId(9));
    let head = e.head_for_test().await;
    let refs = head.indexes.get(index).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for r in refs {
        let k = Key::new(r.key);
        let seg = Segment::open(&**store, &k).await.unwrap();
        out.push((k, seg));
    }
    out
}

#[tokio::test]
async fn a_named_attribute_is_the_one_indexed() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(620);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_text_field("body");
    e.write("idx", (0..24).map(|i| doc(i, "body")).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let segs = segments(&store, t, "idx").await;
    assert_eq!(segs.len(), 1);
    let (key, seg) = &segs[0];
    assert!(
        seg.has_text(),
        "prose in `body` folded to a segment with NO text index, and nothing said so"
    );
    assert_eq!(seg.text_fields(), ["body"]);
    assert!(
        store.get(&text::dict_key(key)).await.is_ok(),
        "the term dictionary sidecar is missing, so the postings are unreachable"
    );

    // ⚠️ The other half: the default is not silently indexing `body` anyway. Without it a
    // `seal` that ignored its argument entirely would pass everything above.
    let t2 = TenantId(621);
    let d = Engine::new(Arc::clone(&store), t2, LaneId(1));
    d.write("idx", (0..24).map(|i| doc(i, "body")).collect())
        .await
        .unwrap();
    d.flush().await.unwrap();
    d.fold().await.unwrap();
    let segs = segments(&store, t2, "idx").await;
    assert!(
        !segs[0].1.has_text(),
        "an engine on the default indexed `body`, so the argument is being ignored"
    );
}

#[tokio::test]
async fn a_compaction_does_not_rebuild_over_the_wrong_field() {
    // ⚠️ The destructive case. `seal` is reached by both `fold` and `compact`, and a
    // compaction RE-ANALYZES the original attribute — postings cannot be inverted back into
    // text. A handle left on the default merging a `body` index would rebuild it over
    // `"text"`, find no strings, and write no postings: an index destroyed by a merge, with
    // every row still there and no error anywhere.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(622);
    let w = Engine::new(Arc::clone(&store), t, LaneId(1)).with_text_field("body");
    for batch in 0..2 {
        w.write(
            "idx",
            (batch * 12..batch * 12 + 12)
                .map(|i| doc(i, "body"))
                .collect(),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        w.fold().await.unwrap();
    }
    assert_eq!(segments(&store, t, "idx").await.len(), 2);

    // The compacting handle is on the DEFAULT, which is the whole point.
    let c = Engine::new(Arc::clone(&store), t, LaneId(2));
    c.compact("idx").await.unwrap();

    let segs = segments(&store, t, "idx").await;
    assert_eq!(segs.len(), 1, "the merge did not produce one segment");
    let (key, seg) = &segs[0];
    assert!(
        seg.has_text(),
        "the merge destroyed the text index: 24 rows still present, no postings"
    );
    assert_eq!(seg.text_fields(), ["body"]);
    assert!(seg.section(Section::Fieldnorms).is_some());
    assert!(store.get(&text::dict_key(key)).await.is_ok());
}

#[tokio::test]
async fn a_compaction_of_disagreeing_inputs_is_refused() {
    // ⚠️ Refused, not resolved. Picking one input's name rebuilds the other's rows over an
    // attribute they do not carry, which is the same silent destruction one segment at a
    // time. Nothing changes a field today — that is the schema this milestone defers — so
    // this is the door being shut before there is a way through it.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(623);
    for (lane, attr) in [(1u64, "body"), (2, text::DEFAULT_TEXT_FIELD)] {
        let e = Engine::new(Arc::clone(&store), t, LaneId(lane)).with_text_field(attr);
        e.write("idx", (0..12).map(|i| doc(i, attr)).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    assert_eq!(segments(&store, t, "idx").await.len(), 2);

    let c = Engine::new(Arc::clone(&store), t, LaneId(3));
    let err = c.compact("idx").await.unwrap_err();
    assert!(
        matches!(err, EngineError::Format(ref m) if m.contains("body") && m.contains("text")),
        "a merge of inputs naming different text fields was not refused by name: {err:?}"
    );
    assert_eq!(
        segments(&store, t, "idx").await.len(),
        2,
        "the refused compaction still committed"
    );
}
