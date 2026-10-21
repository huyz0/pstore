//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What the engine refuses at its own door, beneath any API: a name it reserves, and a read
//! whose consistency and epoch contradict each other. A caller other than the server (a
//! replica, a test, a tool) reaches these with no request validation in front of it.

use pstore_blob::MemoryStore;
use pstore_engine::{Consistency, Engine, EngineError};
use pstore_format::{Document, Value, VectorField};
use pstore_query::{Aggregate, AggregateSpec, OrderBy};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn engine() -> Engine<MemoryStore> {
    Engine::new(Arc::new(MemoryStore::new()), TenantId(98), LaneId(1))
}

fn format_error(r: Result<impl std::fmt::Debug, EngineError>, why: &str) {
    match r {
        Err(EngineError::Format(m)) => assert!(m.contains(why), "{m:?} does not name {why:?}"),
        other => panic!("{why}: {other:?}"),
    }
}

#[tokio::test]
async fn a_document_naming_what_the_engine_reserves_is_refused() {
    let e = engine();
    // `$` names are the query language's operators; an attribute so named could never be
    // filtered on.
    let mut d = Document::new("a", vec![1.0, 0.5]);
    d.attrs.insert("$op".to_owned(), Value::Int(1));
    format_error(e.write("idx", vec![d]).await, "`$op` is reserved");
    // A field ending `$fde` holds an encoding the engine derives from a multi-vector field;
    // a document supplying one would be indexed as the derived encoding.
    let mut d = Document::new("b", vec![1.0, 0.5]);
    d.vectors.insert(
        format!("m{}", pstore_format::fde::FIELD_SUFFIX).into(),
        VectorField::Dense(vec![vec![1.0, 0.5]]),
    );
    format_error(e.write("idx", vec![d]).await, "is reserved");
    // The empty name marks a tombstone: a document carrying it would be a delete.
    let mut d = Document::new("c", vec![1.0, 0.5]);
    d.attrs.insert(String::new(), Value::Int(1));
    format_error(
        e.write("idx", vec![d]).await,
        "an attribute with an empty name is reserved",
    );
    // And nothing was written by any of them.
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let all = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    if let Ok(o) = e.ordered("idx", &all, None, 0, 10, None).await {
        assert!(o.rows.is_empty(), "{:?}", o.rows);
    }
}

#[tokio::test]
async fn a_strong_read_of_a_past_epoch_is_refused() {
    // A past epoch is exactly what it is: a strong read of one would check nothing, and
    // accepting it would let a caller believe it had checked.
    let e = engine();
    e.write("idx", vec![Document::new("a", vec![1.0, 0.5])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    let past = Some(e.fold().await.unwrap());
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let strong = Consistency::Strong;
    let o = e.ordered_as("idx", &by, None, 0, 10, past, strong).await;
    assert!(
        matches!(o, Err(EngineError::Query(ref m)) if m.contains("strong with as_of")),
        "{o:?}"
    );
    let spec = AggregateSpec {
        labels: vec![("n".to_owned(), Aggregate::Count(None))],
        group_by: vec![],
        top_k: 10,
    };
    let a = e
        .aggregate_as("idx", spec.clone(), None, past, strong)
        .await;
    assert!(
        matches!(a, Err(EngineError::Query(ref m)) if m.contains("strong with as_of")),
        "{a:?}"
    );
    // Either alone is a read.
    e.ordered_as("idx", &by, None, 0, 10, past, Consistency::Eventual)
        .await
        .unwrap();
    e.aggregate_as("idx", spec, None, None, strong)
        .await
        .unwrap();
}
