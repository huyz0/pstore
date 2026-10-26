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
use pstore_engine::{Engine, EngineError, Head, Metric, Patch};
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
    // ⚠️ The rung that makes the rest rare: the refused rows are not in a bundle, so the
    // drop-and-count path is a race between two cold processes rather than the normal way to
    // write. ⚠️ Since M35 the refusal is once: the next flush waives it, and the fold
    // quarantines the rows (`a_refused_flush_blocks_no_later_write`).
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
    // ⚠️ M35: refused at the door, where it was accepted and refused at the flush, because a
    // process's first write now reads the recorded schema.
    let other = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("prose");
    let err = other
        .write("docs", vec![texted("b", "prose", "revenue")])
        .await
        .expect_err("a contradicting text field was accepted");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    assert_eq!(
        other.flush().await.unwrap(),
        None,
        "a contradicting text field was written"
    );
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

// ---- M35: a wrong row is refused at the door, and never blocks a lane ----

async fn into(e: &Engine<impl BlobStore + 'static>, index: &str, docs: Vec<Document>) {
    e.write(index, docs).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

async fn ids_in(e: &Engine<impl BlobStore + 'static>, index: &str) -> Vec<String> {
    let mut ids: Vec<String> = e
        .scan(index, None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    ids.sort();
    ids
}

async fn quarantined(e: &Engine<impl BlobStore + 'static>, index: &str) -> Vec<String> {
    let mut ids: Vec<String> = e
        .quarantine(index)
        .await
        .unwrap()
        .map(|q| q.rows.into_iter().map(|r| r.document.id).collect())
        .unwrap_or_default();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_first_write_learns_the_recorded_schema() {
    // ⚠️ The sequence measured while planning M35: a process that had never read HEAD accepted
    // a wrong width, and then refused every correct one, because the door compared against
    // the wrong row it held.
    let store = Arc::new(MemoryStore::new());
    let first = Engine::new(Arc::clone(&store), T, LaneId(1));
    into(&first, "docs", vec![doc("a", 4)]).await;

    let cold = Engine::new(Arc::clone(&store), T, LaneId(2));
    let err = cold
        .write("docs", vec![doc("wrong", 3)])
        .await
        .expect_err("a first write accepted a width HEAD contradicts");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    cold.write("docs", vec![doc("right", 4)]).await.unwrap();
    cold.flush().await.unwrap();
    cold.fold().await.unwrap();
    assert_eq!(ids_in(&cold, "docs").await, ["a", "right"]);
    assert!(committed(&*store).await.schema_rejects.is_empty());
}

#[tokio::test]
async fn a_refused_flush_blocks_no_later_write() {
    // The race the door cannot see: this engine read HEAD before any schema was recorded,
    // and another process records one before this engine flushes.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(2));
    e.write("docs", vec![doc("w1", 3)]).await.unwrap();
    e.write("docs", vec![doc("w2", 3)]).await.unwrap();
    let other = Engine::new(Arc::clone(&store), T, LaneId(1));
    into(&other, "docs", vec![doc("a", 4)]).await;

    let err = e
        .flush()
        .await
        .expect_err("the flush wrote a known conflict");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    // ⚠️ Before M35 this engine refused every later write and flush to `docs` until it
    // restarted: the door compared against the stale rows, and the flush refused them again.
    e.write("docs", vec![doc("right", 4)])
        .await
        .expect("a correct write was refused after a flush's refusal");
    e.flush()
        .await
        .expect("a second flush refused the same rows again");
    e.fold().await.unwrap();
    assert_eq!(ids_in(&e, "docs").await, ["a", "right"]);
    assert_eq!(quarantined(&e, "docs").await, ["w1", "w2"]);

    // ⚠️ And the waiver is spent once a bundle lands. The race again, on an index this engine
    // has no schema for: a refused write's re-read (M9f.2) teaches it the schema, and the
    // stale row it holds is refused at the next flush rather than waived.
    e.write("late", vec![doc("w3", 3)]).await.unwrap();
    into(&other, "late", vec![doc("b", 4)]).await;
    let err = e
        .write("late", vec![doc("w4", 2)])
        .await
        .expect_err("a wrong width was accepted with the schema in hand");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    let err = e
        .flush()
        .await
        .expect_err("a waiver outlived the bundle that spent it");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
}

/// A memory store whose next bundle write answers `Io` without landing, once, and whose next
/// HEAD read fails, once.
#[derive(Debug, Default, Clone)]
struct FailOnce {
    inner: MemoryStore,
    fail: Arc<std::sync::atomic::AtomicBool>,
    fail_head: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl BlobStore for FailOnce {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &pstore_blob::Key) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &pstore_blob::Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(
        &self,
        key: &pstore_blob::Key,
        n: u64,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(
        &self,
        key: &pstore_blob::Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        if key.as_str().ends_with("/HEAD")
            && self
                .fail_head
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(pstore_blob::BlobError::Other(
                "the connection dropped".to_owned(),
            ));
        }
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(
        &self,
        key: &pstore_blob::Key,
    ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &pstore_blob::Key) -> Result<u64, pstore_blob::BlobError> {
        self.inner.head(key).await
    }
    async fn put(
        &self,
        key: &pstore_blob::Key,
        body: bytes::Bytes,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &pstore_blob::Key,
        body: bytes::Bytes,
        pre: pstore_blob::Precondition,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
        if key.as_str().ends_with(".bundle")
            && self.fail.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(pstore_blob::CasError::Io(
                "refused before writing".to_owned(),
            ));
        }
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[pstore_blob::Key]) -> Result<(), pstore_blob::BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(
        &self,
        prefix: &pstore_blob::Key,
    ) -> Result<Vec<pstore_blob::Key>, pstore_blob::BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
    async fn get_range_as(
        &self,
        key: &pstore_blob::Key,
        range: std::ops::Range<u64>,
        class: pstore_blob::Class,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_suffix_as(
        &self,
        key: &pstore_blob::Key,
        n: u64,
        class: pstore_blob::Class,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_suffix_as(key, n, class).await
    }
    async fn get_immutable(
        &self,
        key: &pstore_blob::Key,
        class: pstore_blob::Class,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_immutable(key, class).await
    }
}

#[tokio::test]
async fn one_waiver_covers_every_refused_index() {
    let store = FailOnce::default();
    let e = Engine::new(Arc::new(store.clone()), T, LaneId(2));
    e.write("a", vec![doc("a1", 3)]).await.unwrap();
    e.write("b", vec![doc("b1", 3)]).await.unwrap();
    let other = Engine::new(Arc::new(store.clone()), T, LaneId(1));
    other.write("a", vec![doc("a0", 4)]).await.unwrap();
    other.write("b", vec![doc("b0", 4)]).await.unwrap();
    other.flush().await.unwrap();
    other.fold().await.unwrap();

    // One refusal, naming one index: the check stops at the first conflict.
    let err = e
        .flush()
        .await
        .expect_err("the flush wrote a known conflict");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    // A flush that fails to write its bundle keeps the waiver for its retry.
    store.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = e
        .flush()
        .await
        .expect_err("the injected failure was not hit");
    assert!(matches!(err, EngineError::Blob(_)), "{err:?}");
    e.flush()
        .await
        .expect("the retry refused rows the waiver covers");
    e.fold().await.unwrap();
    assert_eq!(quarantined(&e, "a").await, ["a1"]);
    assert_eq!(quarantined(&e, "b").await, ["b1"]);
    assert_eq!(ids_in(&e, "a").await, ["a0"]);
    assert_eq!(ids_in(&e, "b").await, ["b0"]);
}

#[tokio::test]
async fn the_first_write_reads_head_once() {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let seed = Engine::new(Arc::clone(&store), T, LaneId(1));
    seed.write("docs", vec![doc("a", 4)]).await.unwrap();
    seed.flush().await.unwrap();
    seed.fold().await.unwrap();

    let e = Engine::new(Arc::clone(&store), T, LaneId(3));
    let before = acct.count(T, OpClass::Read);
    e.write("docs", vec![doc("b", 4)]).await.unwrap();
    assert_eq!(acct.count(T, OpClass::Read) - before, 1, "the first write");
    let before = acct.count(T, OpClass::Read);
    for i in 0..10 {
        e.write("docs", vec![doc(&format!("c{i}"), 4)])
            .await
            .unwrap();
    }
    assert_eq!(acct.count(T, OpClass::Read) - before, 0, "ten later writes");
    // The resume's fresh read (M9j) is a different question, and stays.
    let before = acct.count(T, OpClass::Read);
    e.flush().await.unwrap();
    assert!(
        acct.count(T, OpClass::Read) > before,
        "the first flush read nothing"
    );
}

#[tokio::test]
async fn a_failed_first_read_refuses_no_write() {
    // ⚠️ Found by the server's chaos suite: a buffered write that fails on a read fault
    // makes ingest depend on read availability, which it never did. The read is retried by
    // the next write instead.
    let store = FailOnce::default();
    let first = Engine::new(Arc::new(store.clone()), T, LaneId(1));
    into(&first, "docs", vec![doc("a", 4)]).await;

    let cold = Engine::new(Arc::new(store.clone()), T, LaneId(2));
    store
        .fail_head
        .store(true, std::sync::atomic::Ordering::SeqCst);
    cold.write("docs", vec![doc("b", 4)])
        .await
        .expect("a read fault refused a buffered write");
    assert!(
        !store.fail_head.load(std::sync::atomic::Ordering::SeqCst),
        "the first write did not read HEAD"
    );
    let err = cold
        .write("docs", vec![doc("wrong", 3)])
        .await
        .expect_err("the next write did not retry the read");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
}

#[tokio::test]
async fn a_stale_row_of_another_metric_refuses_no_write() {
    // Code review, M35: the schema is authoritative at the door for every property, not the
    // width alone.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(2));
    e.write_as("docs", vec![doc("e", 4)], Metric::EuclideanSquared)
        .await
        .unwrap();
    let other = Engine::new(Arc::clone(&store), T, LaneId(1));
    into(&other, "docs", vec![doc("a", 4)]).await;

    let err = e
        .flush()
        .await
        .expect_err("the flush wrote a known conflict");
    assert!(matches!(err, EngineError::SchemaConflict { .. }), "{err:?}");
    e.write("docs", vec![doc("right", 4)])
        .await
        .expect("a stale row of another metric refused a correct write");
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(ids_in(&e, "docs").await, ["a", "right"]);
    assert_eq!(quarantined(&e, "docs").await, ["e"]);
}

// ---- M36: a row carries its writer's text field, and every fold judges it by that ----

/// The ids a text query over `field` names in `index`, as `e` sees them; empty when refused.
async fn text_hits(
    e: &Engine<impl BlobStore + 'static>,
    index: &str,
    field: &str,
    q: &str,
) -> Vec<String> {
    let legs = vec![pstore_query::Prefetch::Text {
        field: field.to_owned(),
        query: q.to_owned(),
        limit: 10,
    }];
    match e
        .query(index, &legs, pstore_query::Fusion::default(), 10)
        .await
    {
        Ok(answer) => {
            let mut ids: Vec<String> = e.resolve(&answer).into_iter().map(|(id, _)| id).collect();
            ids.sort();
            ids
        }
        Err(_) => Vec::new(),
    }
}

/// Each quarantined row of `index`: its id, and its `$text` stamp if it has one.
async fn stamps(e: &Engine<impl BlobStore + 'static>, index: &str) -> Vec<(String, Option<Value>)> {
    let mut rows: Vec<(String, Option<Value>)> = e
        .quarantine(index)
        .await
        .unwrap()
        .map(|q| {
            q.rows
                .into_iter()
                .map(|r| (r.document.id, r.reserved.get("$text").cloned()))
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn prose(s: &str) -> Option<Value> {
    Some(Value::Str(s.to_owned()))
}

#[tokio::test]
async fn any_fold_quarantines_a_text_field_conflict() {
    // ⚠️ M35 code review measured a waived row sealed with its text unindexed by a foreign
    // fold, and M35 kept the conflict out of its waiver: it then blocked the lane until a
    // restart. Inverted by M36, which stamps the writer's field on the row.
    let store = Arc::new(MemoryStore::new());
    let wrong = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("prose");
    wrong
        .write("docs", vec![texted("p", "prose", "revenue")])
        .await
        .unwrap();
    let right = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    into(&right, "docs", vec![texted("a", "body", "revenue")]).await;

    let err = wrong
        .flush()
        .await
        .expect_err("the flush wrote a known conflict");
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
    wrong
        .flush()
        .await
        .expect("a text-field conflict blocked the lane past one refusal");
    right.fold().await.unwrap();
    assert_eq!(ids_in(&right, "docs").await, ["a"]);
    assert_eq!(
        stamps(&right, "docs").await,
        [("p".to_owned(), prose("prose"))]
    );
    assert_eq!(
        committed(&*store).await.schema_rejects.get("docs").copied(),
        Some(1)
    );
}

/// Engines over `prose` (lane 1) and `body` (lane 2) each write and flush one texted row to
/// `index`, and the `body` engine folds.
async fn race_two_fields(store: &Arc<MemoryStore>, index: &str) -> Engine<MemoryStore> {
    let p = Engine::new(Arc::clone(store), T, LaneId(1)).with_text_field("prose");
    let b = Engine::new(Arc::clone(store), T, LaneId(2)).with_text_field("body");
    p.write(index, vec![texted("p", "prose", "revenue")])
        .await
        .unwrap();
    b.write(index, vec![texted("b", "body", "revenue")])
        .await
        .unwrap();
    p.flush().await.unwrap();
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    b
}

#[tokio::test]
async fn two_writers_text_fields_never_share_an_index() {
    // ⚠️ Spec review, M36, measured on the parent: both rows sealed, the quarantine empty, and
    // one row's text unindexed with no refusal and no count. Lane order decides which field
    // wins; whichever does, the other row is set aside.
    let store = Arc::new(MemoryStore::new());
    let b = race_two_fields(&store, "fresh").await;
    assert_eq!(
        committed(&*store).await.schemas["fresh"].text_field,
        "prose"
    );
    assert_eq!(ids_in(&b, "fresh").await, ["p"]);
    assert_eq!(text_hits(&b, "fresh", "prose", "revenue").await, ["p"]);
    assert_eq!(stamps(&b, "fresh").await, [("b".to_owned(), prose("body"))]);

    // And over an index that already exists with an empty text field (M30).
    let v = Engine::new(Arc::clone(&store), T, LaneId(3));
    into(&v, "vecs", vec![doc("v", 4)]).await;
    assert_eq!(committed(&*store).await.schemas["vecs"].text_field, "");
    let b = race_two_fields(&store, "vecs").await;
    assert_eq!(committed(&*store).await.schemas["vecs"].text_field, "prose");
    assert_eq!(ids_in(&b, "vecs").await, ["p", "v"]);
    assert_eq!(text_hits(&b, "vecs", "prose", "revenue").await, ["p"]);
    assert_eq!(stamps(&b, "vecs").await, [("b".to_owned(), prose("body"))]);
}

#[tokio::test]
async fn a_wrong_row_never_chooses_the_text_field() {
    // Spec review round 2, M36: "the blast radius of a contradiction is the contradicting
    // row". A wrong-width row first in the fold must not pick the field and take every
    // correct row with it.
    let store = Arc::new(MemoryStore::new());
    let p = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("prose");
    let mut wide = doc("w", 8);
    wide.attrs
        .insert("prose".to_owned(), Value::Str("revenue".to_owned()));
    // Before the index exists, so the door cannot know its width.
    p.write("docs", vec![wide]).await.unwrap();
    let v = Engine::new(Arc::clone(&store), T, LaneId(3));
    into(&v, "docs", vec![doc("v", 4)]).await;
    let b = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("body");
    b.write(
        "docs",
        vec![
            texted("b1", "body", "revenue"),
            texted("b2", "body", "revenue"),
        ],
    )
    .await
    .unwrap();
    b.flush().await.unwrap();
    p.flush()
        .await
        .expect_err("the width conflict was not refused once");
    p.flush().await.unwrap();

    b.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["docs"].text_field, "body");
    assert_eq!(ids_in(&b, "docs").await, ["b1", "b2", "v"]);
    assert_eq!(text_hits(&b, "docs", "body", "revenue").await, ["b1", "b2"]);
    assert_eq!(quarantined(&b, "docs").await, ["w"]);
}

#[tokio::test]
async fn a_foreign_fold_indexes_the_writers_field() {
    let store = Arc::new(MemoryStore::new());
    let p = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("prose");
    p.write("docs", vec![texted("p", "prose", "revenue")])
        .await
        .unwrap();
    p.flush().await.unwrap();
    // An engine configured otherwise, which wrote nothing, folds it.
    let b = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("body");
    b.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["docs"].text_field, "prose");
    assert_eq!(text_hits(&b, "docs", "prose", "revenue").await, ["p"]);
}

#[tokio::test]
async fn the_text_stamp_is_never_served() {
    // ⚠️ It guards `$text` left out of `stripped`. M37: a writer over another field than the
    // default stamps every row it writes, a vector-only one included, so a `prose` engine's
    // ordinary `text` attribute is never read as default text (and the default writer's rows
    // carry no stamp: `the_default_field_is_never_stamped`).
    let store = Arc::new(MemoryStore::new());
    let p = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("prose");
    let served = |rows: Vec<Document>| {
        assert!(!rows.is_empty());
        for d in rows {
            assert!(!d.attrs.contains_key("$text"), "{} served its stamp", d.id);
        }
    };
    p.write("docs", vec![texted("p", "prose", "revenue")])
        .await
        .unwrap();
    served(p.scan("docs", None).await.unwrap());
    p.flush().await.unwrap();
    served(p.scan("docs", None).await.unwrap());
    p.fold().await.unwrap();
    served(p.scan("docs", None).await.unwrap());

    // A `prose` engine's vector-only row is stamped too (M37).
    let cold = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("prose");
    cold.write("vecs", vec![doc("w", 2)]).await.unwrap();
    into(&p, "vecs", vec![doc("v", 4)]).await;
    cold.flush()
        .await
        .expect_err("the width conflict was not refused once");
    cold.flush().await.unwrap();
    cold.fold().await.unwrap();
    assert_eq!(
        stamps(&cold, "vecs").await,
        [("w".to_owned(), prose("prose"))]
    );
}

#[tokio::test]
async fn the_field_a_fold_judged_by_is_the_field_it_records() {
    // The row that chose the field is deleted in the same fold. The other writer's row was
    // quarantined against `prose`, so `prose` is recorded: anything else would make that
    // quarantine arbitrary, and let the next fold accept what this one refused. Over an index
    // with an empty field, and over a new one that keeps a vector row.
    let store = Arc::new(MemoryStore::new());
    let v = Engine::new(Arc::clone(&store), T, LaneId(3));
    into(&v, "docs", vec![doc("v", 4)]).await;
    let p = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("prose");
    let b = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("body");
    for index in ["docs", "fresh"] {
        p.write(index, vec![texted("p", "prose", "revenue"), doc("x", 4)])
            .await
            .unwrap();
        p.delete(index, vec!["p".to_owned()]).await.unwrap();
        b.write(index, vec![texted("b", "body", "revenue")])
            .await
            .unwrap();
    }
    p.flush().await.unwrap();
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    for index in ["docs", "fresh"] {
        assert_eq!(quarantined(&b, index).await, ["b"], "{index}");
        assert_eq!(
            committed(&*store).await.schemas[index].text_field,
            "prose",
            "{index}"
        );
    }
}

// ---- M37: an unstamped row was written under the default text field ----

/// A row of width `dims` carrying `text` under `field`.
fn wide_text(id: &str, dims: usize, field: &str, text: &str) -> Document {
    let mut d = doc(id, dims);
    d.attrs
        .insert(field.to_owned(), Value::Str(text.to_owned()));
    d
}

#[tokio::test]
async fn the_default_field_is_never_stamped() {
    // ⚠️ Every server engine uses the default field, so M36's stamp there said nothing and
    // was exactly what a rolling upgrade leaked into segments (BACKLOG row 55).
    let store = Arc::new(MemoryStore::new());
    let cold = Engine::new(Arc::clone(&store), T, LaneId(2));
    cold.write(
        "docs",
        vec![wide_text("t", 2, "text", "revenue"), doc("w", 2)],
    )
    .await
    .unwrap();
    let first = Engine::new(Arc::clone(&store), T, LaneId(1));
    into(&first, "docs", vec![doc("a", 4)]).await;
    cold.flush()
        .await
        .expect_err("the width conflict was not refused once");
    cold.flush().await.unwrap();
    cold.fold().await.unwrap();
    assert_eq!(
        stamps(&cold, "docs").await,
        [("t".to_owned(), None), ("w".to_owned(), None)]
    );
}

#[tokio::test]
async fn an_unstamped_row_is_the_default_fields() {
    // An older build's row, or one buffered past the door: no stamp. Read as the default
    // field's whichever engine folds it, as an absent `$metric` is `dot_product`.
    let store = Arc::new(MemoryStore::new());
    let old = Engine::new(Arc::clone(&store), T, LaneId(1));
    old.write_without_schema_check_for_test("fresh", vec![wide_text("u", 4, "text", "revenue")])
        .await;
    old.flush_without_schema_check_for_test().await.unwrap();
    let b = Engine::new(Arc::clone(&store), T, LaneId(2)).with_text_field("body");
    b.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "text");
    assert_eq!(text_hits(&b, "fresh", "text", "revenue").await, ["u"]);
}

#[tokio::test]
async fn a_custom_writers_row_is_never_the_defaults() {
    // A `body` engine's row with an ordinary attribute named `text`: not text, whichever
    // engine folds it -- and no segment may index it as text behind the schema's back, or a
    // later `body` fold leaves the index uncompactable (M30).
    let store = Arc::new(MemoryStore::new());
    let b = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    b.write("fresh", vec![wide_text("r", 4, "text", "revenue")])
        .await
        .unwrap();
    b.flush().await.unwrap();
    let d = Engine::new(Arc::clone(&store), T, LaneId(2));
    d.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "");
    assert_eq!(ids_in(&d, "fresh").await, ["r"]);

    into(&b, "fresh", vec![texted("s", "body", "revenue")]).await;
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "body");
    b.compact("fresh")
        .await
        .expect("a segment indexed a field the schema does not name");
    assert_eq!(text_hits(&b, "fresh", "body", "revenue").await, ["s"]);
}

#[tokio::test]
async fn a_default_patch_keeps_a_custom_rows_stamp() {
    // ⚠️ Green on the parent, whose `merged` ignores every `$` name: a guard against a patch
    // taking over a row's stamp when it sets no text under its own field.
    let store = Arc::new(MemoryStore::new());
    let b = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    b.write("fresh", vec![wide_text("r", 4, "text", "revenue")])
        .await
        .unwrap();
    b.flush().await.unwrap();
    let d = Engine::new(Arc::clone(&store), T, LaneId(2));
    let colour = Patch {
        id: "r".to_owned(),
        set: std::collections::BTreeMap::from([("color".to_owned(), Value::Str("red".to_owned()))]),
        unset: vec![],
    };
    d.patch("fresh", vec![colour], None).await.unwrap();
    d.flush().await.unwrap();
    d.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "");
}

#[tokio::test]
async fn a_custom_writers_no_op_patch_touches_nothing() {
    // ⚠️ Green on the parent, as above. M13.1: a patch that changes nothing touches nothing --
    // and a stamp is not a change, since a segment row never carries one.
    let store = Arc::new(MemoryStore::new());
    let b = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    into(&b, "idx", vec![texted("x", "body", "revenue")]).await;
    let before = committed(&*store).await.indexes["idx"].clone();
    let same = Patch {
        id: "x".to_owned(),
        set: std::collections::BTreeMap::from([(
            "body".to_owned(),
            Value::Str("revenue".to_owned()),
        )]),
        unset: vec![],
    };
    b.patch("idx", vec![same], None).await.unwrap();
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    assert_eq!(
        committed(&*store).await.indexes["idx"],
        before,
        "a patch that changed nothing wrote a segment or a delete vector"
    );
}

#[tokio::test]
async fn a_patch_by_filter_that_adds_text_fills_the_text_field() {
    // M37: a by-filter patch is stamped as a patch by id is, so the text it adds is read
    // under its writer's field, never the default's.
    let (store, _v, a, _b) = three().await;
    let mut set = std::collections::BTreeMap::new();
    set.insert("body".to_owned(), Value::Str("gamma".to_owned()));
    a.patch_by_filter(
        "idx",
        &pstore_query::Predicate::Absent("nothing".to_owned()),
        Patch {
            id: String::new(),
            set,
            unset: Vec::new(),
        },
    )
    .await
    .unwrap();
    a.flush().await.unwrap();
    a.fold().await.unwrap();
    assert_eq!(text_field(&store).await, "body");
    assert_eq!(body(&a, "gamma").await, ["v"]);
}

#[tokio::test]
async fn a_sealed_custom_row_keeps_its_field_across_a_patch() {
    // ⚠️ M37 code review, reproduced: the seal strips the stamp, so a `body` row read back from
    // its segment looked unstamped -- the default's -- and a later patch of `color` made its
    // ordinary `text` attribute fill the field, after which every `body` row was a conflict.
    let store = Arc::new(MemoryStore::new());
    let b = Engine::new(Arc::clone(&store), T, LaneId(1)).with_text_field("body");
    into(&b, "fresh", vec![wide_text("r", 4, "text", "revenue")]).await;
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "");
    let colour = |id: &str| Patch {
        id: id.to_owned(),
        set: std::collections::BTreeMap::from([("color".to_owned(), Value::Str("red".to_owned()))]),
        unset: vec![],
    };
    b.patch("fresh", vec![colour("r")], None).await.unwrap();
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "");
    // And by filter, which reaches `merged` through another path.
    b.patch_by_filter(
        "fresh",
        &pstore_query::Predicate::Absent("nothing".to_owned()),
        Patch {
            id: String::new(),
            ..colour("")
        },
    )
    .await
    .unwrap();
    b.flush().await.unwrap();
    b.fold().await.unwrap();
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "");
    into(&b, "fresh", vec![texted("s", "body", "revenue")]).await;
    assert_eq!(committed(&*store).await.schemas["fresh"].text_field, "body");
}

// ---- M41: a fresh view never matches another writer's row ----

/// Every engine's text-query answer for `revenue` over `text` and over `body`, as a `Result`
/// so an answer turning into an error is a difference, never a panic.
async fn answers(engines: &[&Engine<MemoryStore>]) -> Vec<Result<Vec<String>, String>> {
    let mut out = Vec::new();
    for e in engines {
        for field in ["text", "body"] {
            out.push(text_query(e, "idx", field, "revenue").await);
        }
    }
    out
}

async fn text_query(
    e: &Engine<MemoryStore>,
    index: &str,
    field: &str,
    q: &str,
) -> Result<Vec<String>, String> {
    let legs = vec![pstore_query::Prefetch::Text {
        field: field.to_owned(),
        query: q.to_owned(),
        limit: 10,
    }];
    e.query(index, &legs, pstore_query::Fusion::default(), 10)
        .await
        .map(|a| e.resolve(&a).into_iter().map(|(id, _)| id).collect())
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn a_fresh_view_never_matches_another_writers_row() {
    // BACKLOG row 56's last item said a default engine's fresh view could match a `body`
    // writer's ordinary `text` attribute until a fold. A fresh view holds only its own
    // engine's rows, so it cannot: measured while planning M41, and pinned here.
    let (store, _v, b, _t) = three().await;
    let d = Engine::new(Arc::clone(&store), T, LaneId(4));
    let mut row = doc("r", 4);
    row.attrs
        .insert("text".to_owned(), Value::Str("revenue".to_owned()));
    b.write("idx", vec![row]).await.unwrap();
    b.flush().await.unwrap();
    let before = answers(&[&d, &b]).await;
    assert!(
        before.iter().all(|a| a == &Ok(Vec::new())),
        "before the fold: {before:?}"
    );
    d.fold().await.unwrap();
    assert_eq!(
        answers(&[&d, &b]).await,
        before,
        "an answer changed across the fold"
    );
    assert_eq!(text_field(&store).await, "");

    // And for text that is indexed: the writer's fresh view finds it under its own field,
    // and so does the fold, run by an engine configured otherwise.
    b.write("idx", vec![texted("s", "body", "revenue")])
        .await
        .unwrap();
    b.flush().await.unwrap();
    assert_eq!(
        text_query(&b, "idx", "body", "revenue").await,
        Ok(vec!["s".to_owned()])
    );
    d.fold().await.unwrap();
    assert_eq!(text_field(&store).await, "body");
    assert_eq!(
        text_query(&d, "idx", "body", "revenue").await,
        Ok(vec!["s".to_owned()])
    );
}
