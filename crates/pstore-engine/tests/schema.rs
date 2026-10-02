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
    let segs = |keys: Vec<pstore_blob::Key>| -> Vec<String> {
        keys.into_iter()
            .map(|k| k.as_str().to_owned())
            .filter(|k| k.ends_with(".seg"))
            .collect()
    };
    let all = pstore_blob::Key::new(String::new());
    let before = segs(store.list_unrestricted(&all).await.unwrap());
    let writes = acct.count(T, OpClass::Write);
    cold.fold().await.unwrap();
    // It must not have sealed a segment for the contradicting index. ⚠️ M25: it does write
    // one object -- the quarantine the rejected row is set aside in -- beside the HEAD commit,
    // so the count is exact and the segments are checked directly.
    assert_eq!(
        acct.count(T, OpClass::Write) - writes,
        2,
        "a contradicting fold writes the HEAD commit and its quarantine, and nothing else"
    );
    assert_eq!(segs(store.list_unrestricted(&all).await.unwrap()), before);
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
    // ⚠️ Exactly one read since M9f.2: a refusal re-reads HEAD once before it stands, because
    // after an index is dropped and made again the cached schema is stale. An accepted write
    // still costs nothing.
    assert_eq!(
        acct.count(T, OpClass::Read) - before,
        1,
        "a refusal is one HEAD read, no more"
    );
    let before = acct.count(T, OpClass::Read);
    e.write("docs", vec![doc("right", 4)]).await.unwrap();
    assert_eq!(
        acct.count(T, OpClass::Read),
        before,
        "an accepted write issued a request"
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

// ---- M30: the schema's text field decides where a segment's text is indexed ----

/// A vector-only fold, then engines configured `body` and `title`, on one store.
async fn three() -> (
    Arc<MemoryStore>,
    Engine<MemoryStore>,
    Engine<MemoryStore>,
    Engine<MemoryStore>,
) {
    let store = Arc::new(MemoryStore::new());
    let v = Engine::new(Arc::clone(&store), T, LaneId(1));
    let a = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("body");
    let b = Engine::new(Arc::clone(&store), T, LaneId(3)).with_text_field("title");
    folded(&v, vec![doc("v", 4)]).await;
    assert_eq!(text_field(&store).await, "");
    (store, v, a, b)
}

async fn folded(e: &Engine<MemoryStore>, docs: Vec<Document>) {
    e.write("idx", docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

async fn text_field(store: &MemoryStore) -> String {
    committed(store).await.schemas["idx"].text_field.clone()
}

/// The ids a `body` text query names, as `e` sees them.
async fn body(e: &Engine<MemoryStore>, q: &str) -> Vec<String> {
    let legs = vec![pstore_query::Prefetch::Text {
        field: "body".to_owned(),
        query: q.to_owned(),
        limit: 10,
    }];
    let answer = e
        .query("idx", &legs, pstore_query::Fusion::default(), 10)
        .await
        .unwrap();
    e.resolve(&answer).into_iter().map(|(id, _)| id).collect()
}

/// `e` reads HEAD, as any query does, so its door knows the recorded schema.
async fn refresh(e: &Engine<MemoryStore>) {
    let _ = body(e, "anything").await;
}

#[tokio::test]
async fn an_empty_text_field_is_filled_by_the_first_fold_with_text() {
    let (store, v, a, b) = three().await;
    folded(&a, vec![texted("a", "body", "alpha")]).await;
    assert_eq!(
        text_field(&store).await,
        "body",
        "the first fold with text filled nothing"
    );
    refresh(&b).await;
    let err = b
        .write("idx", vec![texted("b", "title", "beta")])
        .await
        .expect_err("a second text field was accepted");
    assert!(
        matches!(
            err,
            EngineError::SchemaConflict {
                what: "the text field",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        v.compact("idx").await.unwrap().is_some(),
        "nothing compacted"
    );
    assert_eq!(body(&a, "alpha").await, ["a"]);
}

#[tokio::test]
async fn a_patch_that_adds_text_fills_the_text_field() {
    let (store, _v, a, _b) = three().await;
    let mut set = std::collections::BTreeMap::new();
    set.insert("body".to_owned(), Value::Str("gamma".to_owned()));
    a.patch(
        "idx",
        vec![pstore_engine::Patch {
            id: "v".to_owned(),
            set,
            unset: Vec::new(),
        }],
        None,
    )
    .await
    .unwrap();
    a.flush().await.unwrap();
    a.fold().await.unwrap();
    assert_eq!(
        text_field(&store).await,
        "body",
        "a patch's text filled nothing"
    );
    assert_eq!(body(&a, "gamma").await, ["v"]);
}

#[tokio::test]
async fn a_fold_indexes_the_schemas_text_field_not_its_own() {
    let (store, _v, a, b) = three().await;
    folded(&a, vec![texted("a", "body", "alpha")]).await;
    assert_eq!(text_field(&store).await, "body");
    refresh(&b).await;
    folded(&b, vec![texted("x", "body", "gamma")]).await;
    assert_eq!(
        body(&a, "gamma").await,
        ["x"],
        "a fold by a process configured otherwise wrote no postings for the index's field"
    );
}

#[tokio::test]
async fn the_fresh_segment_indexes_the_schemas_text_field() {
    let (_store, _v, a, b) = three().await;
    folded(&a, vec![texted("a", "body", "alpha")]).await;
    refresh(&b).await;
    b.write("idx", vec![texted("x", "body", "gamma")])
        .await
        .unwrap();
    assert_eq!(
        body(&b, "gamma").await,
        ["x"],
        "the unfolded row was indexed over the process's field"
    );
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    assert_eq!(body(&b, "gamma").await, ["x"]);
}

#[tokio::test]
async fn a_compaction_rebuilds_over_the_schemas_text_field() {
    // Segments that name no text field -- sealed by a `title` process over `body` rows, which
    // fills nothing -- under a schema recording `body`, set by hand: an index from before M30.
    let (store, v, _a, b) = three().await;
    folded(&b, vec![texted("x", "body", "gamma")]).await;
    folded(&b, vec![texted("y", "body", "gamma delta")]).await;
    assert_eq!(text_field(&store).await, "");
    v.commit_head_for_test(|h| {
        h.schemas.get_mut("idx").unwrap().text_field = "body".to_owned();
    })
    .await
    .unwrap();
    assert!(
        v.compact("idx").await.unwrap().is_some(),
        "nothing compacted"
    );
    let mut got = body(&v, "gamma").await;
    got.sort();
    assert_eq!(
        got,
        ["x", "y"],
        "the merge was not built over the schema's field"
    );
}

#[tokio::test]
async fn a_compaction_refuses_an_input_of_another_field() {
    // Inputs naming only `title`, under a schema recording `body` (set by hand).
    let (store, v, _a, b) = three().await;
    folded(&b, vec![texted("x", "title", "gamma")]).await;
    folded(&b, vec![texted("y", "title", "delta")]).await;
    v.commit_head_for_test(|h| {
        h.schemas.get_mut("idx").unwrap().text_field = "body".to_owned();
    })
    .await
    .unwrap();
    let before = committed(&*store).await;
    let err = v
        .compact("idx")
        .await
        .expect_err("a merge was built over a field the schema does not record");
    assert!(matches!(err, EngineError::Format(_)), "{err:?}");
    let after = committed(&*store).await;
    assert_eq!(after.epoch, before.epoch, "HEAD moved");
    assert_eq!(after.indexes["idx"].len(), before.indexes["idx"].len());
}

#[tokio::test]
async fn a_vector_only_fold_fills_no_text_field() {
    let (store, _v, _a, b) = three().await;
    folded(&b, vec![doc("w", 4)]).await;
    assert_eq!(
        text_field(&store).await,
        "",
        "a fold without text filled the field"
    );
}

#[tokio::test]
async fn a_vector_only_segment_does_not_refuse_a_text_query() {
    // M30: the server's own sequence, with its default text field. The index's first fold
    // has no text, and its second does: a text query must answer, not refuse.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    folded(&e, vec![doc("v", 4)]).await;
    folded(&e, vec![texted("t", "text", "alpha")]).await;
    let legs = vec![pstore_query::Prefetch::Text {
        field: "text".to_owned(),
        query: "alpha".to_owned(),
        limit: 10,
    }];
    let answer = e
        .query("idx", &legs, pstore_query::Fusion::default(), 10)
        .await
        .expect("a text query over an index with a vector-only segment was refused");
    let ids: Vec<String> = e.resolve(&answer).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["t"]);
}

#[tokio::test]
async fn a_cached_fresh_segment_is_rebuilt_when_the_text_field_is_filled() {
    // `b` holds an unfolded `body` row, indexed while the schema was `""` -- over its own field,
    // so with no postings. Another process's fold then fills `body`. `b`'s rows are unchanged,
    // but its cached fresh segment is over the wrong field now, and must be rebuilt.
    let (store, _v, a, b) = three().await;
    b.write("idx", vec![texted("x", "body", "gamma")])
        .await
        .unwrap();
    assert!(body(&b, "gamma").await.is_empty());
    folded(&a, vec![texted("a", "body", "alpha")]).await;
    assert_eq!(text_field(&store).await, "body");
    assert_eq!(
        body(&b, "gamma").await,
        ["x"],
        "a fresh segment cached under the old text field was served"
    );
}

#[tokio::test]
async fn a_compaction_with_no_field_to_keep_builds_no_text_index() {
    // Code review, M30: one process configured `body` folds rows carrying a `text` attribute,
    // which it does not index, so nothing fills. A merge that fell back to the default field
    // would index `text` without recording it -- and the first `body` fold would then make
    // the index uncompactable. With no field recorded and none named, it indexes nothing.
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("body");
    folded(&a, vec![doc("v", 4)]).await;
    folded(&a, vec![texted("x", "text", "gamma")]).await;
    folded(&a, vec![texted("y", "text", "delta")]).await;
    assert!(
        a.compact("idx").await.unwrap().is_some(),
        "nothing compacted"
    );
    assert_eq!(text_field(&store).await, "");
    folded(&a, vec![texted("z", "body", "alpha")]).await;
    assert_eq!(text_field(&store).await, "body");
    assert!(
        a.compact("idx").await.unwrap().is_some(),
        "the index became uncompactable"
    );
    assert_eq!(body(&a, "alpha").await, ["z"]);
}

#[tokio::test]
async fn before_any_text_the_fresh_segment_indexes_the_process_field() {
    // Code review, M30: a schema recording `""` names no field, so the unfolded rows are
    // indexed over this process's, as the fold that seals them -- and fills it -- will be.
    let (_store, _v, _a, b) = three().await;
    b.write("idx", vec![texted("x", "title", "gamma")])
        .await
        .unwrap();
    let legs = vec![pstore_query::Prefetch::Text {
        field: "title".to_owned(),
        query: "gamma".to_owned(),
        limit: 10,
    }];
    let answer = b
        .query("idx", &legs, pstore_query::Fusion::default(), 10)
        .await
        .unwrap();
    let ids: Vec<String> = b.resolve(&answer).into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        ids,
        ["x"],
        "an empty recorded field was used as a field name"
    );
}
