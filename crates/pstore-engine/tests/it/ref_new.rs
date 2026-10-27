//! One condition per operation (M133): the engine half of `$ref_new`, whose references the
//! server resolves before the engine sees them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::MemoryStore;
use pstore_engine::{
    Applied, Consistency, Declared, Engine, EngineError, Metric, OperationId, Outcome, Patch,
    applying,
};
use pstore_format::{Document, Value};
use pstore_query::{Op, OrderBy, Predicate};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;

const T: TenantId = TenantId(133);

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

fn n_lt(n: i64) -> Predicate {
    Predicate::Cmp("n".to_owned(), Op::Lt, Value::Int(n))
}

fn set_n(id: &str, n: i64) -> Patch {
    Patch {
        id: id.to_owned(),
        set: BTreeMap::from([("n".to_owned(), Value::Int(n))]),
        unset: vec![],
    }
}

/// Every row of `idx`, as `id -> n`.
async fn rows(e: &Engine<MemoryStore>) -> BTreeMap<String, i64> {
    let by_id = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    e.ordered_as("idx", &by_id, None, 0, 1000, None, Consistency::Strong)
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|d| match d.attrs.get("n") {
            Some(Value::Int(n)) => (d.id, *n),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn known(a: Applied) -> Outcome {
    match a {
        Applied::Known(o, _) => o,
        other => panic!("{other:?}"),
    }
}

/// Each operation is judged by its own condition: `a` and `b` are both at 5, and only the
/// operations whose condition admits 5 apply, in upserts and in patches.
#[tokio::test]
async fn each_operation_carries_its_own_condition() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    e.write(
        "idx",
        vec![doc("a", 5), doc("b", 5), doc("c", 5), doc("d", 5)],
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let op = OperationId(1);
    let mark = e.mark().await.unwrap();
    applying(op, async {
        e.write_conditioned(
            "idx",
            vec![doc("a", 7), doc("b", 3)],
            vec![n_lt(7), n_lt(3)],
            Metric::DotProduct,
            &Declared::default(),
        )
        .await
        .unwrap();
        e.patch_conditioned(
            "idx",
            vec![set_n("c", 9), set_n("d", 1)],
            vec![n_lt(9), n_lt(1)],
        )
        .await
        .unwrap();
    })
    .await;
    e.flush().await.unwrap();
    let got = known(e.await_applied(mark, op).await.unwrap());
    assert_eq!(
        (got.upserted, got.patched, got.skipped),
        (1, 1, 2),
        "{got:?}"
    );
    assert_eq!(
        rows(&e).await,
        BTreeMap::from([
            ("a".to_owned(), 7),
            ("b".to_owned(), 5),
            ("c".to_owned(), 9),
            ("d".to_owned(), 5),
        ])
    );
}

/// Criterion 8: a count of conditions unlike the count of operations is refused, and
/// nothing is buffered.
#[tokio::test]
async fn lengths_must_match() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    let err = e
        .write_conditioned(
            "idx",
            vec![doc("a", 1), doc("b", 1)],
            vec![n_lt(1)],
            Metric::DotProduct,
            &Declared::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Format(_)), "{err:?}");
    let err = e
        .patch_conditioned("idx", vec![set_n("a", 1)], vec![n_lt(1), n_lt(2)])
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Format(_)), "{err:?}");
    e.flush().await.unwrap();
    assert_eq!(e.next_seq(), None, "something was buffered");
}
