//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The trigram declaration where the API cannot reach (M15.2): a compaction re-seals with it,
//! and the engine refuses `id` itself.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::{Declared, Engine, Metric};
use pstore_format::{Document, Value};
use pstore_query::{OrderBy, Pattern, PatternKind, Predicate};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(71);

fn doc(i: usize) -> Document {
    let v = if i == 777 {
        "zzqqvv".to_owned()
    } else {
        format!("value {} {}", i % 97, i % 13)
    };
    let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.5]);
    d.attrs.insert("s".to_owned(), Value::Str(v.clone()));
    d.attrs.insert("u".to_owned(), Value::Str(v));
    d
}

#[tokio::test]
async fn a_compaction_keeps_the_sketch() {
    let store = Arc::new(Accounted::new(MemoryStore::new()));
    let e = Engine::new(Arc::new(store.as_tenant(T)), T, LaneId(1));
    let declared = Declared {
        fts: None,
        trigram: Some(vec!["s".to_owned()]),
    };
    for k in 0..3 {
        let docs = (k * 500..(k + 1) * 500).map(doc).collect();
        e.write_declared("idx", docs, Metric::DotProduct, None, &declared)
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    assert!(
        e.compact("idx").await.unwrap().is_some(),
        "nothing compacted"
    );
    assert_eq!(e.head_for_test().await.schemas["idx"].trigram, ["s"]);
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let mut read = Vec::new();
    for attr in ["s", "u"] {
        let f = Predicate::Pattern(
            attr.to_owned(),
            Pattern::new(PatternKind::Glob, "zzq*").unwrap(),
        );
        let before = store.bytes(T, OpClass::Read);
        let got = e.ordered("idx", &by, Some(&f), 0, 10, None).await.unwrap();
        read.push(store.bytes(T, OpClass::Read) - before);
        let ids: Vec<&str> = got.rows.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["d00777"], "{attr}");
    }
    assert!(
        read[0] < read[1],
        "after compaction: {read:?} bytes, declared then undeclared"
    );
}

#[tokio::test]
async fn the_engine_refuses_to_sketch_id() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    let declared = Declared {
        fts: None,
        trigram: Some(vec!["id".to_owned()]),
    };
    let got = e
        .write_declared("idx", vec![doc(1)], Metric::DotProduct, None, &declared)
        .await;
    assert!(got.is_err());
    assert_eq!(e.flush().await.unwrap(), None);
}
