//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The schema: who sets it, whether it may change, and what a disagreement does — M7d.
//!
//! ⚠️ **The shape of this milestone was decided by one measurement in spec review**: refusing
//! a contradicting fold would stop every later fold for the whole tenant, forever, because a
//! fold is all-or-nothing across every index in the bundle set and re-reads the same bundles
//! on every attempt. One accepted API call would brick a tenant. So the ladder is: refuse at
//! the door, refuse at the flush, and at the fold **drop and count** — never stop.

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_engine::{Engine, EngineError, Head};
use pstore_format::{Document, Value};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(11);

fn doc(id: &str, dims: usize) -> Document {
    Document::new(id, (0..dims).map(|i| i as f32).collect())
}

fn texted(id: &str, field: &str, text: &str) -> Document {
    let mut d = doc(id, 4);
    d.attrs
        .insert(field.to_owned(), Value::Str(text.to_owned()));
    d
}

/// HEAD as it is committed, decoded from the object rather than from the engine that wrote it.
async fn committed<S: BlobStore>(store: &S) -> Head {
    Head::decode(&store.get(&Head::key(T)).await.unwrap()).unwrap()
}

#[tokio::test]
async fn a_fold_records_the_schema_it_inferred() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    e.write("docs", vec![texted("a", "body", "quarterly revenue")])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let head = committed(&*store).await;
    let schema = head.schemas.get("docs").expect("the first fold records it");
    assert_eq!(schema.dims, 4, "the width comes from the rows");
    assert_eq!(schema.text_field, "body");
    assert!(head.schema_rejects.is_empty());
}

#[tokio::test]
async fn a_vector_only_index_records_no_text_field() {
    // ⚠️ `seal` builds a text index only for rows that carry the field, so an index of pure
    // vectors must not record the process's knob as though it meant something -- a later
    // process configured differently would then be refused over a field neither segment has
    // postings for. Spec review found this one.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    e.write("vecs", vec![doc("a", 4)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["vecs"].text_field, "");

    // And a differently-configured process may fold into it, because there is nothing to
    // disagree about.
    let other = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("prose");
    other.write("vecs", vec![doc("b", 4)]).await.unwrap();
    other.flush().await.unwrap();
    other.fold().await.unwrap();
    assert_eq!(committed(&*store).await.indexes["vecs"].len(), 2);
}

#[tokio::test]
async fn a_wrong_width_row_does_not_stop_the_tenant() {
    // ⚠️ **The criterion this milestone exists in its second draft for.** One contradicting
    // row, durable in a lane bundle, written by a process that never read HEAD.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let first = Engine::new(Arc::clone(&store), T, LaneId(1));
    first.write("docs", vec![doc("a", 4)]).await.unwrap();
    first.flush().await.unwrap();
    first.fold().await.unwrap();

    // A second process, cold: it has read nothing, so its door check has nothing to compare
    // against. Its flush is what should refuse -- and here we go around it deliberately, to
    // build the state the fold has to survive.
    let cold = Engine::new(Arc::clone(&store), T, LaneId(2));
    cold.write_without_schema_check_for_test("docs", vec![doc("wrong", 2)])
        .await;
    cold.write("other", vec![doc("fine", 3)]).await.unwrap();
    cold.flush_without_schema_check_for_test().await.unwrap();

    let before = committed(&*store).await.epoch;
    let epoch = cold
        .fold()
        .await
        .expect("a contradiction must not fail the fold");
    assert!(epoch > before, "the fold committed nothing: {epoch:?}");

    let head = committed(&*store).await;
    assert_eq!(
        head.schema_rejects.get("docs").copied(),
        Some(1),
        "the discarded row was not counted, so it was discarded silently"
    );
    // ⚠️ The other index in the SAME bundle folded and is queryable: the drop is per index,
    // not per fold.
    assert_eq!(head.indexes["other"].len(), 1);
    assert_eq!(head.indexes["docs"].len(), 1, "no segment for the bad rows");
    // ⚠️ And the tenant keeps working: a later, correct write folds normally. The behaviour
    // this replaces would have failed here and on every fold after it, forever.
    cold.write("docs", vec![doc("later", 4)]).await.unwrap();
    cold.flush().await.unwrap();
    cold.fold().await.unwrap();
    assert_eq!(committed(&*store).await.indexes["docs"].len(), 2);
}

#[tokio::test]
async fn a_contradicting_index_seals_nothing() {
    // The refusal is before `seal`, so no object is written that no HEAD will ever name --
    // the orphan M6e exists to reap. Asserted on the request counter, because the epoch is
    // local until the commit and cannot show the difference.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 4)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let cold = Engine::new(Arc::clone(&store), T, LaneId(2));
    cold.write_without_schema_check_for_test("docs", vec![doc("wrong", 2)])
        .await;
    cold.flush_without_schema_check_for_test().await.unwrap();
    let writes = acct.count(T, OpClass::Write);
    cold.fold().await.unwrap();
    // The fold still commits HEAD and may write a graveyard entry, but it must not have
    // sealed a segment for the contradicting index: a segment is a PUT plus its sidecars.
    assert!(
        acct.count(T, OpClass::Write) - writes <= 1,
        "a contradicting fold wrote {} objects; one is the HEAD commit",
        acct.count(T, OpClass::Write) - writes
    );
}

#[tokio::test]
async fn a_flush_refuses_before_the_bundle_is_written() {
    // ⚠️ The rung that makes the rest safe: a row that never becomes durable can never reach
    // a fold, so the drop-and-count path is a race between two cold processes rather than the
    // normal way to write.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 4)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let cold = Engine::new(Arc::clone(&store), T, LaneId(2));
    cold.write_without_schema_check_for_test("docs", vec![doc("wrong", 2)])
        .await;
    let writes = acct.count(T, OpClass::Write);
    let err = cold
        .flush()
        .await
        .expect_err("a flush wrote a bundle contradicting the schema");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    assert_eq!(
        acct.count(T, OpClass::Write),
        writes,
        "the refusal happened after the bundle was written"
    );
}

#[tokio::test]
async fn the_schema_is_read_once_per_process() {
    // ⚠️ A schema read per flush would be a request per write, which is the cost model this
    // whole design exists to protect.
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let seed = Engine::new(Arc::clone(&store), T, LaneId(1));
    seed.write("docs", vec![doc("a", 4)]).await.unwrap();
    seed.flush().await.unwrap();
    seed.fold().await.unwrap();

    let e = Engine::new(Arc::clone(&store), T, LaneId(3));
    e.write("docs", vec![doc("b", 4)]).await.unwrap();
    let before = acct.count(T, OpClass::Read);
    e.flush().await.unwrap();
    let first = acct.count(T, OpClass::Read) - before;
    e.write("docs", vec![doc("c", 4)]).await.unwrap();
    let before = acct.count(T, OpClass::Read);
    e.flush().await.unwrap();
    let second = acct.count(T, OpClass::Read) - before;
    assert!(first > second, "first flush {first} reads, second {second}");
    assert_eq!(second, 0, "a later flush read {second} objects");
}

#[tokio::test]
async fn the_write_door_uses_the_schema_the_process_has_read() {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let seed = Engine::new(Arc::clone(&store), T, LaneId(1));
    seed.write("docs", vec![doc("a", 4)]).await.unwrap();
    seed.flush().await.unwrap();
    seed.fold().await.unwrap();

    let e = Engine::new(Arc::clone(&store), T, LaneId(4));
    // Any HEAD read warms the cache -- here the one the API makes on every enumeration.
    e.indexes().await.unwrap();
    let before = acct.count(T, OpClass::Read);
    let err = e
        .write("docs", vec![doc("wrong", 2)])
        .await
        .expect_err("the door let a wrong width through with the schema in hand");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    assert_eq!(
        acct.count(T, OpClass::Read),
        before,
        "the door check issued a request"
    );
}

#[tokio::test]
async fn a_text_field_that_contradicts_the_schema_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    e.write("docs", vec![texted("a", "body", "revenue")])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    // ⚠️ A second process configured over a different attribute, with rows that carry it: its
    // segment would hold postings nobody queries, which is M6c's failure at index scope.
    let other = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("prose");
    other
        .write("docs", vec![texted("b", "prose", "revenue")])
        .await
        .unwrap();
    let err = other
        .flush()
        .await
        .expect_err("a contradicting text field was written");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
}

#[tokio::test]
async fn a_wrong_row_anywhere_in_the_batch_is_caught() {
    // ⚠️ **Code review found this by probing**: the check read `docs.first()`, so a wrong-width
    // row that was not first went straight into the segment. A fold merges every lane's rows
    // for an index into one batch ordered by lane, so "first" is not even the caller's first
    // -- and a mixed-width segment is unrefusable afterwards, because a segment declares one
    // width and scores every row it holds against it.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 4)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let err = e
        .write("docs", vec![doc("ok", 4), doc("wrong", 2)])
        .await
        .expect_err("a wrong row in second position was accepted");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");

    // And the whole batch is refused rather than half-written: a partially accepted batch is
    // a write the caller cannot reason about.
    assert!(e.pending_for_test().await.is_empty());
}

#[tokio::test]
async fn only_the_contradicting_rows_are_dropped() {
    // ⚠️ **The second thing code review measured**: the fold dropped every row of the index,
    // so one wrong row from a cold process discarded the correct rows another writer had
    // flushed and had acknowledged -- writers that passed both the door and the flush. The
    // watermark then advances past their bundles and GC reaps them, so the loss is permanent.
    let store = Arc::new(MemoryStore::new());
    let seed = Engine::new(Arc::clone(&store), T, LaneId(1));
    seed.write("docs", vec![doc("seed", 4)]).await.unwrap();
    seed.flush().await.unwrap();
    seed.fold().await.unwrap();

    let good = Engine::new(Arc::clone(&store), T, LaneId(3));
    good.write("docs", vec![doc("good1", 4), doc("good2", 4)])
        .await
        .unwrap();
    good.flush().await.unwrap();

    let cold = Engine::new(Arc::clone(&store), T, LaneId(5));
    cold.write_without_schema_check_for_test("docs", vec![doc("wrong", 2)])
        .await;
    cold.flush_without_schema_check_for_test().await.unwrap();

    cold.fold().await.unwrap();
    let head = committed(&*store).await;
    assert_eq!(
        head.schema_rejects.get("docs").copied(),
        Some(1),
        "the count must be the contradicting rows, not the index's whole batch"
    );
    let rows: u32 = head.indexes["docs"].iter().map(|s| s.rows).sum();
    assert_eq!(
        rows, 3,
        "the two acknowledged correct rows were discarded with the wrong one"
    );
    let ids = cold.scan("docs", None).await.unwrap();
    let mut names: Vec<&str> = ids.iter().map(|d| d.id.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["good1", "good2", "seed"]);
}

#[tokio::test]
async fn an_all_dropped_fold_still_commits_what_it_learned() {
    // ⚠️ The case the spec singles out: if every row in the set contradicts, the fold must
    // still commit -- advancing the watermark and recording the count -- or the discard is
    // invisible and the bundles are read again on every later fold.
    let store = Arc::new(MemoryStore::new());
    let seed = Engine::new(Arc::clone(&store), T, LaneId(1));
    seed.write("docs", vec![doc("seed", 4)]).await.unwrap();
    seed.flush().await.unwrap();
    seed.fold().await.unwrap();
    let before = committed(&*store).await;

    let cold = Engine::new(Arc::clone(&store), T, LaneId(7));
    cold.write_without_schema_check_for_test("docs", vec![doc("wrong", 2)])
        .await;
    cold.flush_without_schema_check_for_test().await.unwrap();
    cold.fold().await.unwrap();

    let after = committed(&*store).await;
    assert!(after.epoch > before.epoch, "the fold committed nothing");
    assert_eq!(after.schema_rejects.get("docs").copied(), Some(1));
    assert!(
        after.watermarks.contains_key(&7),
        "lane 7's watermark did not advance, so its bundle is read again forever: {:?}",
        after.watermarks
    );
    assert_eq!(after.indexes["docs"].len(), 1, "no segment was added");
}

#[tokio::test]
async fn a_brand_new_index_cannot_be_created_mixed_width() {
    // ⚠️ Spotted by code review beside the two defects it blocked on, and closed here rather
    // than filed: an index with no schema and no rows had nothing to compare a batch against,
    // so a single mixed-width batch created it. The schema then recorded the first row's
    // width and every other row read back at a width it never had.
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    let err = e
        .write("fresh", vec![doc("a", 4), doc("b", 2)])
        .await
        .expect_err("a mixed-width batch created an index");
    assert!(
        matches!(
            err,
            EngineError::DimensionMismatch {
                expected: 4,
                got: 2
            }
        ),
        "{err:?}"
    );
    assert!(e.pending_for_test().await.is_empty());

    // A consistent batch still creates it, so the check refuses only what it should.
    e.write("fresh", vec![doc("a", 4), doc("b", 4)])
        .await
        .unwrap();
}

#[tokio::test]
async fn a_conforming_fold_records_no_rejects() {
    // ⚠️ The reject count is recorded only when something was dropped. M8g's sweep found
    // `dropped > 0` -> `>=` surviving: every fold over an index with a schema would then commit
    // `schema_rejects[idx] = 0` into HEAD. The first fold records the schema; the second is the
    // one that checks against it.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("docs", vec![doc("a", 4)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("docs", vec![doc("b", 4)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let head = committed(&*store).await;
    assert!(
        head.schemas.contains_key("docs"),
        "the index has no schema to check against"
    );
    assert!(
        head.schema_rejects.is_empty(),
        "a fold that dropped nothing recorded rejects: {:?}",
        head.schema_rejects
    );
}
