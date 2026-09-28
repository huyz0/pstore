//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

//! An index's analyzer survives what the API cannot reach (M14.1): a compaction, which
//! re-analyzes the stored text.

use pstore_blob::MemoryStore;
use pstore_engine::{Engine, Metric};
use pstore_format::text::{Analyzer, FullText};
use pstore_format::{Document, Value};
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(id: &str, text: &str) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs
        .insert("text".to_owned(), Value::Str(text.to_owned()));
    d
}

async fn find(e: &Engine<MemoryStore>, q: &str) -> Vec<String> {
    let legs = vec![Prefetch::Text {
        field: "text".to_owned(),
        query: q.to_owned(),
        limit: 10,
    }];
    let answer = e.query("idx", &legs, Fusion::default(), 10).await.unwrap();
    e.resolve(&answer).into_iter().map(|(id, _)| id).collect()
}

#[tokio::test]
async fn a_compaction_keeps_the_analyzer() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(60), LaneId(1));
    let fts = FullText {
        analyzer: Analyzer {
            stemming: true,
            ..Analyzer::default()
        },
        ..FullText::default()
    };
    for (id, t) in [
        ("r", "he runs daily"),
        ("s", "she sings"),
        ("t", "they ran"),
    ] {
        e.write_with("idx", vec![doc(id, t)], Metric::DotProduct, &fts)
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    assert_eq!(find(&e, "running").await, ["r"]);
    assert!(
        e.compact("idx").await.unwrap().is_some(),
        "nothing compacted"
    );
    assert_eq!(
        find(&e, "running").await,
        ["r"],
        "compaction re-analyzed with the default"
    );
}

#[tokio::test]
async fn the_fresh_view_follows_an_analyzer_another_process_recorded() {
    // This process holds an unfolded row declared with stemming, and has served it. Another
    // process then creates the index with the default: this one's rows have not changed, but
    // the analyzer its fresh view must use has, so the view it cached is stale.
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), TenantId(61), LaneId(1));
    let b = Engine::new(Arc::clone(&store), TenantId(61), LaneId(2));
    let stem = FullText {
        analyzer: Analyzer {
            stemming: true,
            ..Analyzer::default()
        },
        ..FullText::default()
    };
    a.write_with(
        "idx",
        vec![doc("r", "he runs daily")],
        Metric::DotProduct,
        &stem,
    )
    .await
    .unwrap();
    assert_eq!(find(&a, "running").await, ["r"]);
    b.write("idx", vec![doc("s", "she sings")]).await.unwrap();
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    assert!(
        find(&a, "running").await.is_empty(),
        "the fresh view kept the stemming analyzer"
    );
    assert_eq!(find(&a, "runs").await, ["r"]);
}
