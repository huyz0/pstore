//! A by-filter operation is bounded (M136): above `with_filter_rows`, counted over the
//! index the fold read or at the operation's place in fold order, it is refused whole.
//! M137: unless it allows partial application, when it applies up to the bound and its
//! outcome says rows remain.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::MemoryStore;
use pstore_engine::{Applied, ByFilter, Engine, OperationId, Outcome, Patch, applying};
use pstore_format::{Document, Value};
use pstore_query::{Op, Predicate};
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;

const BOUND: usize = 3;

fn engine() -> Engine<MemoryStore> {
    Engine::new(Arc::new(MemoryStore::new()), TenantId(136), LaneId(1)).with_filter_rows(BOUND)
}

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

fn n_is(n: i64) -> Predicate {
    Predicate::Cmp("n".to_owned(), Op::Eq, Value::Int(n))
}

fn set_n(n: i64) -> Patch {
    Patch {
        id: String::new(),
        set: BTreeMap::from([("n".to_owned(), Value::Int(n))]),
        unset: vec![],
    }
}

/// `ones` rows with `n = 1`, then one with `n = 2`, folded.
async fn seeded(e: &Engine<MemoryStore>, ones: usize) {
    let mut docs: Vec<Document> = (0..ones).map(|i| doc(&format!("r{i}"), 1)).collect();
    docs.push(doc("two", 2));
    e.write("idx", docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

/// Runs `f` as one applied write and returns what its fold decided.
async fn applied<F: std::future::Future<Output = ()>>(e: &Engine<MemoryStore>, f: F) -> Outcome {
    let op = OperationId(1);
    let mark = e.mark().await.unwrap();
    applying(op, f).await;
    e.flush().await.unwrap();
    match e.await_applied(mark, op).await.unwrap() {
        Applied::Known(o, _) => o,
        other => panic!("not a known outcome: {other:?}"),
    }
}

/// Every row, by id, with its `n`.
async fn rows(e: &Engine<MemoryStore>) -> Vec<(String, Option<Value>)> {
    let mut rows: Vec<_> = e
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| (d.id, d.attrs.get("n").cloned()))
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn outcome(upserted: u64, deleted: u64, patched: u64, rejected: u64) -> Outcome {
    Outcome {
        upserted,
        deleted,
        patched,
        skipped: 0,
        rejected,
        rows_remaining: false,
    }
}

/// What a pass of an `allow_partial` operation changed, and whether rows remain.
fn pass(deleted: u64, patched: u64, rows_remaining: bool) -> Outcome {
    Outcome {
        deleted,
        patched,
        rows_remaining,
        ..Outcome::default()
    }
}

fn set(name: &str, n: i64) -> Patch {
    Patch {
        id: String::new(),
        set: BTreeMap::from([(name.to_owned(), Value::Int(n))]),
        unset: vec![],
    }
}

#[tokio::test]
async fn a_filter_admitting_the_bound_applies() {
    let e = engine();
    seeded(&e, BOUND).await;
    let got = applied(&e, async {
        e.delete_by_filter("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, BOUND as u64, 0, 0));
    assert_eq!(rows(&e).await, [("two".to_owned(), Some(Value::Int(2)))]);
}

#[tokio::test]
async fn a_filter_admitting_more_than_the_bound_is_refused_whole() {
    let e = engine();
    seeded(&e, BOUND + 1).await;
    let before = rows(&e).await;
    let got = applied(&e, async {
        e.write("idx", vec![doc("new", 7)]).await.unwrap();
        e.delete_by_filter("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, outcome(1, 0, 0, 1));
    let mut want = before.clone();
    want.push(("new".to_owned(), Some(Value::Int(7))));
    want.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(rows(&e).await, want);

    // And a patch by filter: no row changes.
    let got = applied(&e, async {
        e.patch_by_filter("idx", &n_is(1), set_n(5)).await.unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 0, 0, 1));
    assert_eq!(rows(&e).await, want);
}

#[tokio::test]
async fn fold_order_counts() {
    // The index admits only the bound; an earlier patch in the same write adds one more.
    let e = engine();
    seeded(&e, BOUND).await;
    let got = applied(&e, async {
        e.patch_by_filter("idx", &n_is(2), set_n(1)).await.unwrap();
        e.delete_by_filter("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 0, 1, 1));
    let rows = rows(&e).await;
    assert_eq!(rows.len(), BOUND + 1);
    assert!(rows.iter().all(|(_, n)| *n == Some(Value::Int(1))));
}

#[tokio::test]
async fn the_scan_counts() {
    // The index admits one more than the bound; an earlier delete in the same write removes
    // one, so at its place in fold order the filter admits the bound. Still refused.
    let e = engine();
    seeded(&e, BOUND + 1).await;
    let got = applied(&e, async {
        e.delete("idx", vec!["r0".to_owned()]).await.unwrap();
        e.delete_by_filter("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 1, 0, 1));
    assert_eq!(rows(&e).await.len(), BOUND + 1);
}

#[tokio::test]
async fn the_fold_holds_at_most_one_row_past_the_bound() {
    let e = engine();
    seeded(&e, 10).await;
    let got = applied(&e, async {
        e.delete_by_filter("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 0, 0, 1));
    // One filter, so exactly the bound is retained, whatever order the segments are read in.
    assert_eq!(e.kept_for_test(), BOUND);
}

#[tokio::test]
async fn each_operation_reads_its_own_scan_count() {
    // As `the_scan_counts`, after a by-filter operation within the bound: the delete by
    // filter is the second by-filter operation, and its scan count is its own.
    let e = engine();
    seeded(&e, BOUND + 1).await;
    let got = applied(&e, async {
        e.delete("idx", vec!["r0".to_owned()]).await.unwrap();
        e.patch_by_filter("idx", &n_is(2), set_n(3)).await.unwrap();
        e.delete_by_filter("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 1, 1, 1));
    assert_eq!(rows(&e).await.len(), BOUND + 1);
}

#[tokio::test]
async fn a_partial_delete_resumes_until_nothing_remains() {
    let e = engine();
    seeded(&e, 7).await;
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(
            applied(&e, async {
                e.delete_by_filter_partial("idx", &n_is(1)).await.unwrap();
            })
            .await,
        );
    }
    assert_eq!(got, [pass(3, 0, true), pass(3, 0, true), pass(1, 0, false)]);
    assert_eq!(rows(&e).await, [("two".to_owned(), Some(Value::Int(2)))]);
}

#[tokio::test]
async fn a_partial_patch_counts_only_rows_it_would_change() {
    // Seven rows the filter admits, two of which already hold the patched value.
    let e = engine();
    seeded(&e, 7).await;
    e.patch(
        "idx",
        vec![
            Patch {
                id: "r0".to_owned(),
                ..set("m", 9)
            },
            Patch {
                id: "r1".to_owned(),
                ..set("m", 9)
            },
        ],
        None,
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let mut got = Vec::new();
    for _ in 0..2 {
        got.push(
            applied(&e, async {
                e.patch_by_filter_partial("idx", &n_is(1), set("m", 9))
                    .await
                    .unwrap();
            })
            .await,
        );
    }
    assert_eq!(got, [pass(0, 3, true), pass(0, 2, false)]);
    let m = e.scan("idx", None).await.unwrap();
    assert!(
        m.iter()
            .filter(|d| d.id != "two")
            .all(|d| d.attrs.get("m") == Some(&Value::Int(9)))
    );
}

#[tokio::test]
async fn a_partial_operation_at_the_bound_leaves_nothing() {
    let e = engine();
    seeded(&e, BOUND).await;
    let got = applied(&e, async {
        e.delete_by_filter_partial("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, pass(BOUND as u64, 0, false));
}

#[tokio::test]
async fn a_partial_operation_counts_in_fold_order() {
    // The index admits the bound; an earlier patch by filter makes a fourth match.
    let e = engine();
    seeded(&e, BOUND).await;
    let got = applied(&e, async {
        e.patch_by_filter("idx", &n_is(2), set_n(1)).await.unwrap();
        e.delete_by_filter_partial("idx", &n_is(1)).await.unwrap();
    })
    .await;
    assert_eq!(got, pass(BOUND as u64, 1, true));
    assert_eq!(rows(&e).await.len(), 1);
}

#[tokio::test]
async fn a_partial_operation_takes_only_rows_it_would_change() {
    // r0 already holds the value, and a per-id patch of another attribute in the same
    // write names it, so the fold retains it; the partial patch must pass over it.
    let e = engine();
    seeded(&e, BOUND + 1).await;
    e.patch(
        "idx",
        vec![Patch {
            id: "r0".to_owned(),
            ..set("m", 9)
        }],
        None,
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let got = applied(&e, async {
        e.patch(
            "idx",
            vec![Patch {
                id: "r0".to_owned(),
                ..set("k", 5)
            }],
            None,
        )
        .await
        .unwrap();
        e.patch_by_filter_partial("idx", &n_is(1), set("m", 9))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, pass(0, BOUND as u64 + 1, false));
    let all = e.scan("idx", None).await.unwrap();
    assert!(
        all.iter()
            .filter(|d| d.id != "two")
            .all(|d| d.attrs.get("m") == Some(&Value::Int(9)))
    );
}

#[tokio::test]
async fn an_operations_own_bound_applies() {
    // M139: an operation bounded at 2, under an engine bound of 3.
    let two = |partial| ByFilter {
        partial,
        rows: Some(2),
    };
    let e = engine();
    seeded(&e, BOUND).await;
    let got = applied(&e, async {
        e.delete_by_filter_bounded("idx", &n_is(1), two(false))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 0, 0, 1));
    assert_eq!(e.kept_for_test(), 2);
    let got = applied(&e, async {
        e.delete_by_filter_bounded("idx", &n_is(1), two(true))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, pass(2, 0, true));
    assert_eq!(rows(&e).await.len(), 2);
}

#[tokio::test]
async fn the_smaller_bound_wins() {
    let e = engine();
    seeded(&e, BOUND + 1).await;
    let got = applied(&e, async {
        e.patch_by_filter_bounded(
            "idx",
            &n_is(1),
            set_n(5),
            ByFilter {
                partial: false,
                rows: Some(5),
            },
        )
        .await
        .unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 0, 0, 1));
    // And in fold order: the index admits the bound, an earlier patch adds a fourth.
    let e = engine();
    seeded(&e, BOUND).await;
    let five = ByFilter {
        partial: false,
        rows: Some(5),
    };
    let got = applied(&e, async {
        e.patch_by_filter("idx", &n_is(2), set_n(1)).await.unwrap();
        e.delete_by_filter_bounded("idx", &n_is(1), five)
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, outcome(0, 0, 1, 1));
}
