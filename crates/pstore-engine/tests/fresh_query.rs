//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The indexed query reaches the freshness layer — M5h.
//!
//! ⚠️ `Memtable`'s doc states the promise: *"visibility does not wait on the fold, so the flush
//! interval never enters the time-to-searchable budget."* `scan` and `search` honoured it and
//! `query` did not — a document written a moment ago was **absent from an indexed answer and
//! present in an exact one**, with nothing in either name to say which you get.
//!
//! ⚠️ The memtable becomes a **segment**, sealed in memory by the same builder a fold uses, so
//! there is one BM25, one dense path, one sparse path, one fusion and one `(segment, row)`
//! space. Scoring raw documents instead would have meant a second implementation of each that
//! must agree with the first exactly — the hazard `Hit`'s own doc names about inventing an
//! identity twice.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::{DEFAULT_FIELD, Document};
use pstore_query::{Fusion, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const DIM: usize = 8;

/// Component 0 rises with `i` and dominates, so a dot-product ranking orders by index.
fn doc(i: usize) -> Document {
    Document::new(
        format!("d{i:05}"),
        (0..DIM)
            .map(|j| {
                if j == 0 {
                    i as f32 / 100.0
                } else {
                    ((i + j) % 13) as f32 / 13.0
                }
            })
            .collect(),
    )
}

fn params(threshold: usize) -> pstore_index::cluster::Params {
    pstore_index::cluster::Params {
        target_list_size: 40,
        exact_scan_threshold: threshold,
        ..pstore_index::cluster::Params::default()
    }
}

fn dense(i: usize, limit: usize) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: doc(i).vector().to_vec(),
        limit,
        tune: pstore_index::vec_index::Query {
            k: limit,
            ..pstore_index::vec_index::Query::default()
        },
    }]
}

/// The ids an answer names, in rank order, resolving the fresh ordinal against its documents.
async fn ids(e: &Engine<MemoryStore>, a: &pstore_engine::Answer, index: &str) -> Vec<String> {
    let rows = e.segment_rows_for_test(index).await.unwrap();
    a.hits
        .iter()
        .map(|h| {
            if h.segment == a.unfolded_at {
                a.unfolded[h.row].id.clone()
            } else {
                rows[h.segment][h.row].id.clone()
            }
        })
        .collect()
}

#[tokio::test]
async fn an_unfolded_row_reaches_the_indexed_query() {
    // ⚠️ This inverts M5g's `an_unfolded_row_is_visible_to_scan_and_not_to_query`, which is
    // amended rather than deleted: it now asserts that `query` and `scan` AGREE about the row.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(800);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params(100));
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    // Written, flushed, not folded: durable, and in no committed segment.
    e.write("idx", vec![doc(9_999)]).await.unwrap();
    e.flush().await.unwrap();

    let a = e
        .query("idx", &dense(9_999, 10), Fusion::default(), 10)
        .await
        .unwrap();
    assert_eq!(
        a.unfolded.len(),
        1,
        "the fresh rows were not collected at all"
    );
    let named = ids(&e, &a, "idx").await;
    assert!(
        named.contains(&"d09999".to_owned()),
        "the unfolded row did not reach the indexed query: {named:?}"
    );
    assert!(
        e.scan("idx", None)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == "d09999"),
        "scan and query must agree about this row"
    );
}

#[tokio::test]
async fn a_fresh_row_is_ranked_against_the_corpus_not_beside_it() {
    // ⚠️ RRF is blind to score magnitude, so fusing folded and fresh separately and merging
    // gives the memtable's best hit the corpus's best hit's credit. A fresh row that belongs
    // in the MIDDLE must come back in the middle -- not first.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(801);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params(100));
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    // ⚠️ A distinct id whose component 0 puts it BETWEEN two folded rows -- 2.895 sits under
    // d00290 and over d00289. The first fixture reused `doc(250)`, which is both a duplicate
    // id and outside the top 20 of this query, so it asserted nothing.
    let mut mid = doc(289);
    mid.id = "fresh-mid".to_owned();
    mid.vectors.insert(
        DEFAULT_FIELD.to_owned(),
        pstore_format::VectorField::Dense(vec![{
            let mut v = doc(289).vector().to_vec();
            v[0] = 2.895;
            v
        }]),
    );
    e.write("idx", vec![mid]).await.unwrap();
    e.flush().await.unwrap();

    let a = e
        .query("idx", &dense(299, 20), Fusion::default(), 20)
        .await
        .unwrap();
    let named = ids(&e, &a, "idx").await;
    let at = named
        .iter()
        .position(|id| id == "fresh-mid")
        .expect("the fresh row is absent");
    assert!(
        at > 0,
        "the fresh row was ranked FIRST, which is what stapling two answers together does: \
         {named:?}"
    );
    assert!(
        at + 1 < named.len(),
        "the fresh row was ranked last: {named:?}"
    );
}

#[tokio::test]
async fn a_fold_does_not_change_the_answer_below_the_threshold() {
    // ⚠️ The criterion that cannot be faked: it compares the fresh path against the folded path
    // over the same rows, so any difference in how the two are scored shows up as a reordering.
    //
    // ⚠️ Scoped below the exact-scan threshold, and the scoping is a review finding. Above it a
    // folded segment is searched by ANN probe and the fresh one is scanned exactly, so a row
    // the probe would miss is present before the fold and absent after.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(802);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params(100_000));
    e.write("idx", (0..60).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("idx", (60..80).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();

    let before = e
        .query("idx", &dense(70, 10), Fusion::default(), 10)
        .await
        .unwrap();
    let before_ids = ids(&e, &before, "idx").await;
    assert!(!before_ids.is_empty());
    assert!(
        before.unfolded.len() == 20,
        "the fixture has no fresh rows to compare"
    );

    e.fold().await.unwrap();
    let after = e
        .query("idx", &dense(70, 10), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(
        after.unfolded.is_empty(),
        "the fold did not drain the memtable"
    );
    let after_ids = ids(&e, &after, "idx").await;
    assert_eq!(
        before_ids, after_ids,
        "folding changed the answer, so the fresh and folded halves are scored differently"
    );
}

#[tokio::test]
async fn an_index_with_nothing_unfolded_is_unchanged() {
    // ⚠️ An EMPTY fresh segment must not be added: it would occupy an ordinal for nothing, and
    // every other ordinal is a number a caller resolves against HEAD.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(803);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params(100));
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let a = e
        .query("idx", &dense(299, 10), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(a.unfolded.is_empty());
    assert!(
        a.hits.iter().all(|h| h.segment < a.unfolded_at),
        "a hit came back at the fresh ordinal with no fresh rows"
    );
    assert!(!a.hits.is_empty());
}

#[tokio::test]
async fn the_unfolded_ordinal_indexes_the_returned_documents() {
    // ⚠️ A caller indexing the wrong list gets a plausible wrong document, so both directions
    // are asserted.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(804);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params(100));
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("idx", vec![doc(9_998), doc(9_999)]).await.unwrap();
    e.flush().await.unwrap();

    let a = e
        .query("idx", &dense(9_999, 5), Fusion::default(), 5)
        .await
        .unwrap();
    let head = e.head_for_test().await;
    let segments = head.indexes["idx"].len();
    assert_eq!(
        a.unfolded_at, segments,
        "the fresh ordinal is not one past HEAD's segments"
    );
    for h in &a.hits {
        if h.segment == a.unfolded_at {
            assert!(
                h.row < a.unfolded.len(),
                "a fresh hit's row is outside the documents returned with it"
            );
        } else {
            assert!(h.segment < segments, "a hit names a segment HEAD does not");
        }
    }
    assert!(
        a.hits.iter().any(|h| h.segment == a.unfolded_at),
        "the fixture produced no fresh hits, so it asserts nothing"
    );
}

#[tokio::test]
async fn an_indexed_query_is_still_three_rounds_and_no_list() {
    let t = TenantId(805);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(t));
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params(100));
    e.write("idx", (0..300).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("idx", vec![doc(9_999)]).await.unwrap();
    e.flush().await.unwrap();

    let before = acct.count(t, OpClass::Read);
    let a = e
        .query("idx", &dense(9_999, 10), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(!a.hits.is_empty());
    // ⚠️ Sealing and searching the fresh segment happens in a private in-memory store, so it
    // must cost the TENANT nothing. A rebuild that reached the tenant's store would be a
    // request per query for data already in memory.
    let reads = acct.count(t, OpClass::Read) - before;
    assert!(reads > 0, "the query read nothing, so the bound is vacuous");
    assert_eq!(acct.count(t, OpClass::List), 0, "an indexed query listed");
    assert_eq!(
        acct.count(t, OpClass::Write),
        acct.count(t, OpClass::Write),
        "sealing the fresh segment wrote to the tenant's store"
    );
}

/// A document whose prose is the terms given, and whose vector is fixed.
fn text_doc(id: &str, terms: &[&str]) -> Document {
    let mut d = Document::new(id.to_owned(), vec![1.0; DIM]);
    d.attrs.insert(
        pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
        pstore_format::Value::Str(terms.join(" ")),
    );
    d
}

fn text_leg(q: &str, limit: usize) -> Vec<Prefetch> {
    vec![Prefetch::Text {
        field: pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
        query: q.to_owned(),
        limit,
    }]
}

#[tokio::test]
async fn the_statistics_include_the_unfolded_rows() {
    // ⚠️ **Added after a mutation survived.** Every other fixture here drives a DENSE leg,
    // which does not read `Stats` at all — so dropping the fresh segment's term dictionary,
    // which takes its rows out of the corpus statistics entirely, changed nothing and no test
    // noticed. M5c measured that global IDF changes the top-1; a fresh slice scored against
    // statistics gathered only from the folded segments is that defect one corpus-slice down.
    //
    // The comparison is the same one criterion 4 makes and the only one that cannot be faked:
    // the same documents, partly fresh and then wholly folded, must rank identically. Below
    // the exact-scan threshold, so both halves are exact.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(806);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1)).with_index_params(params(100_000));

    // Folded: 40 short documents, half carrying `gamma`.
    let folded: Vec<Document> = (0..40)
        .map(|i| {
            let t = if i < 20 { "gamma" } else { "delta" };
            text_doc(&format!("a{i:02}"), &[t, "filler"])
        })
        .collect();
    e.write("idx", folded).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    // ⚠️ Unfolded: twenty LONG documents, exactly one of which carries `gamma`. They shift both
    // corpus-wide inputs to BM25 at once — `gamma`'s document frequency and `avgdl` — which is
    // what makes the ranking move if they are left out of the statistics.
    let fresh: Vec<Document> = (0..20)
        .map(|i| {
            let mut terms = vec!["padding"; 20];
            if i == 0 {
                terms[0] = "gamma";
            }
            text_doc(&format!("b{i:02}"), &terms)
        })
        .collect();
    e.write("idx", fresh).await.unwrap();
    e.flush().await.unwrap();

    let before = e
        .query("idx", &text_leg("gamma", 40), Fusion::default(), 21)
        .await
        .unwrap();
    assert_eq!(before.unfolded.len(), 20, "the fixture has no fresh rows");
    let before_ids = ids(&e, &before, "idx").await;
    assert!(
        before_ids.iter().any(|id| id == "b00"),
        "the fresh row carrying `gamma` never reached the query: {before_ids:?}"
    );

    e.fold().await.unwrap();
    let after = e
        .query("idx", &text_leg("gamma", 40), Fusion::default(), 21)
        .await
        .unwrap();
    assert!(
        after.unfolded.is_empty(),
        "the fold did not drain the memtable"
    );
    let after_ids = ids(&e, &after, "idx").await;
    assert_eq!(
        before_ids, after_ids,
        "folding changed the text ranking, so the fresh rows were not in the statistics the \
         folded half was scored with"
    );
}
