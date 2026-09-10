//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! One query over N segments — M5f.2.
//!
//! ⚠️ **Three confidently wrong rankings, none of which fails.** Per-segment IDF undoes D-30
//! (M5c measured that global IDF changes the top-1). RRF fused per segment and then merged
//! gives the top hit of *every* segment the same credit, because RRF is blind to score
//! magnitude by design. And a segment loop awaited in sequence returns the identical answer at
//! N times the depth. What separates right from wrong here is the ranking, the round count and
//! the error on a missing sidecar — so that is what is asserted.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{Document, Section, Segment, SegmentWriter, Value, text};
use pstore_query::{Fusion, Hit, Prefetch, QueryError, Target, query};
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;

/// A document whose text is exactly the terms given.
fn doc(id: &str, terms: &[&str]) -> Document {
    let mut d = Document::new(id.to_owned(), vec![1.0, 0.0]);
    d.attrs.insert(
        text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str(terms.join(" ")),
    );
    d
}

/// Writes one text segment and its dictionary, returning the target that reaches it.
async fn put<S: BlobStore>(store: &S, name: &str, docs: &[Document]) -> Target {
    let built = text::build(docs, text::DEFAULT_TEXT_FIELD);
    let key = Key::new(format!("t/idx/{name}.seg"));
    let mut w = SegmentWriter::new(8);
    for d in docs {
        w.push(d.clone());
    }
    store
        .put(
            &key,
            w.with_section(Section::TextPostings, built.postings)
                .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
                .try_finish()
                .unwrap(),
        )
        .await
        .unwrap();
    store
        .put(&text::dict_key(&key), bytes::Bytes::from(built.dictionary))
        .await
        .unwrap();
    Target {
        centroids: Key::new(format!("t/idx/{name}.centroids")),
        segment: key,
    }
}

/// The document ids a result names, in rank order.
async fn ids<S: BlobStore>(store: &S, targets: &[Target], hits: &[Hit]) -> Vec<String> {
    let mut rows: Vec<Vec<Document>> = Vec::new();
    for t in targets {
        let seg = Segment::open(store, &t.segment).await.unwrap();
        rows.push(seg.scan(store, &t.segment, None).await.unwrap());
    }
    hits.iter()
        .map(|h| rows[h.segment][h.row].id.clone())
        .collect()
}

fn text_leg(q: &str, limit: usize) -> Vec<Prefetch> {
    vec![Prefetch::Text {
        field: text::DEFAULT_TEXT_FIELD.to_owned(),
        query: q.to_owned(),
        limit,
    }]
}

/// ⚠️ A corpus whose two halves disagree about **both** inputs to BM25 that are corpus-wide:
/// how common `gamma` is, and how long a document is.
///
/// - `a**` — 40 **short** documents of two terms. Half carry `gamma`.
/// - `b**` — 40 **long** documents of twenty terms. Exactly one carries `gamma`.
///
/// Scored globally, `gamma` has one idf and one `avgdl`, and a short document wins on length
/// normalisation. Scored per segment, `gamma` is common in A and unique in B, and each half's
/// documents are of average length for their own half — so B's single hit wins instead. The
/// order **flips**, which is what makes the criterion able to fail.
fn split_corpus() -> (Vec<Document>, Vec<Document>) {
    let a: Vec<Document> = (0..40)
        .map(|i| {
            let t = if i < 20 { "gamma" } else { "delta" };
            doc(&format!("a{i:02}"), &[t, "filler"])
        })
        .collect();
    let b: Vec<Document> = (0..40)
        .map(|i| {
            let mut terms = vec!["padding"; 20];
            if i == 0 {
                terms[0] = "gamma";
            }
            doc(&format!("b{i:02}"), &terms)
        })
        .collect();
    (a, b)
}

#[tokio::test]
async fn global_statistics_change_the_ranking_across_segments() {
    let (a, b) = split_corpus();
    let split = MemoryStore::new();
    let two = [put(&split, "a", &a).await, put(&split, "b", &b).await];

    let one_store = MemoryStore::new();
    let all: Vec<Document> = a.iter().chain(&b).cloned().collect();
    let one = [put(&one_store, "all", &all).await];

    let q = text_leg("gamma", 40);
    // ⚠️ 21, so segment B's single hit must appear. Under global statistics it is LAST —
    // twenty short A documents outrank one long B document — and under per-segment
    // statistics it is first. The flip is the assertion.
    let split_hits = query(&split, &two, &q, Fusion::Rrf { k: 60.0 }, 21)
        .await
        .unwrap();
    let one_hits = query(&one_store, &one, &q, Fusion::Rrf { k: 60.0 }, 21)
        .await
        .unwrap();

    // ⚠️ Compared by document id, not by row: the two layouts number rows differently by
    // construction, so a row comparison would fail on a fixture where nothing is wrong.
    let split_ids = ids(&split, &two, &split_hits).await;
    let one_ids = ids(&one_store, &one, &one_hits).await;
    assert!(!split_ids.is_empty(), "the fixture matched nothing");
    assert_eq!(
        split_ids, one_ids,
        "two segments ranked differently from the same documents in one -- the statistics \
         are not global"
    );

    // ⚠️ The half that makes the criterion able to fail. Equality alone passes over a fixture
    // whose per-segment statistics happen to agree with the global ones, which is the trap
    // M5c's ledger records finding -- and it passed here too until the fixture was rebuilt to
    // disagree about `avgdl` as well as about `df`.
    //
    // Under global statistics a SHORT document of segment A wins: `gamma` has one idf, and
    // length normalisation favours the two-term document over the twenty-term one. Under
    // per-segment statistics `gamma` is common in A and unique in B, and each half's
    // documents are of average length for their own half, so B's single hit wins instead.
    assert!(
        split_ids[0].starts_with('a'),
        "the top hit is {}, which is what PER-SEGMENT statistics rank first",
        split_ids[0]
    );
    assert_eq!(
        split_ids.last().map(String::as_str),
        Some("b00"),
        "segment B's single hit is not last, which is where GLOBAL statistics put it: \
         {split_ids:?}"
    );
}

/// A corpus whose **best** hit is in the second segment: twenty two-term documents in A, and
/// a single one-term document in B. Length normalisation puts B's first.
fn best_hit_in_the_second_segment() -> (Vec<Document>, Vec<Document>) {
    let a: Vec<Document> = (0..20)
        .map(|i| doc(&format!("a{i:02}"), &["gamma", "filler"]))
        .collect();
    let mut b: Vec<Document> = vec![doc("b00", &["gamma"])];
    b.extend((1..20).map(|i| doc(&format!("b{i:02}"), &vec!["padding"; 20])));
    (a, b)
}

#[tokio::test]
async fn each_retrievers_union_is_ranked_before_it_is_fused() {
    // ⚠️ `fuse` reads a leg's POSITION in the vec as its rank. So concatenating the segments'
    // answers without re-ranking hands segment 0's hits ranks 1..n and segment 1's ranks
    // n+1.., which ranks a whole segment above another for no reason a caller could see —
    // and with a single retriever the final order is exactly the leg's input order, so
    // nothing downstream repairs it.
    //
    // This fixture is the one that can tell: the best hit is in segment B, so score order and
    // concatenation order disagree. On a fixture where segment A happens to hold every good
    // hit they agree, and the missing sort is invisible — which it was, until this test.
    let (a, b) = best_hit_in_the_second_segment();
    let s = MemoryStore::new();
    let two = [put(&s, "a", &a).await, put(&s, "b", &b).await];

    let hits = query(&s, &two, &text_leg("gamma", 40), Fusion::Rrf { k: 60.0 }, 5)
        .await
        .unwrap();
    let ranked = ids(&s, &two, &hits).await;
    assert_eq!(
        ranked.first().map(String::as_str),
        Some("b00"),
        "the top hit is {:?}, which is what the UNSORTED concatenation ranks first",
        ranked.first()
    );
    assert!(
        hits.windows(2).all(|w| w[0].score >= w[1].score),
        "the fused answer is not in rank order"
    );
}

#[tokio::test]
async fn fusion_happens_once_over_the_union() {
    // Two retrievers over two segments. ⚠️ Fusing per segment and merging gives segment B's
    // best hit the same 1/(k+1) as segment A's, however much worse it is -- so the wrong
    // implementation surfaces one "winner" per segment at equal score.
    let (a, b) = split_corpus();
    let s = MemoryStore::new();
    let two = [put(&s, "a", &a).await, put(&s, "b", &b).await];

    let both = vec![
        Prefetch::Text {
            field: text::DEFAULT_TEXT_FIELD.to_owned(),
            query: "gamma".to_owned(),
            limit: 20,
        },
        Prefetch::Text {
            field: text::DEFAULT_TEXT_FIELD.to_owned(),
            query: "delta".to_owned(),
            limit: 20,
        },
    ];
    let hits = query(&s, &two, &both, Fusion::Rrf { k: 60.0 }, 8)
        .await
        .unwrap();
    assert!(hits.len() > 1);
    // The scores must not be the flat "one winner per segment" pattern: with fusion computed
    // once, the top hits' RRF contributions come from global ranks and differ.
    let distinct = hits
        .iter()
        .map(|h| format!("{:.9}", h.score))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        distinct.len() > 1,
        "every hit scored identically, which is what per-segment fusion produces: {hits:?}"
    );
    assert!(
        hits.windows(2).all(|w| w[0].score >= w[1].score),
        "the union is not in rank order"
    );
}

#[tokio::test]
async fn four_segments_cost_one_open_round_and_one_leg_round() {
    let (a, b) = split_corpus();
    let counting = DepthCounting::new(MemoryStore::new());
    let mut targets = Vec::new();
    for (i, docs) in [&a, &b, &a, &b].iter().enumerate() {
        targets.push(put(&counting, &format!("s{i}"), docs).await);
    }
    let q = text_leg("gamma delta", 40);

    counting.reset();
    let many = query(&counting, &targets, &q, Fusion::Rrf { k: 60.0 }, 5)
        .await
        .unwrap();
    let deep = counting.depth();

    counting.reset();
    query(&counting, &targets[..1], &q, Fusion::Rrf { k: 60.0 }, 5)
        .await
        .unwrap();
    let one = counting.depth();

    assert!(!many.is_empty());
    // ⚠️ Equal to the one-segment depth, which is the form that does not depend on the
    // fixture. A loop over segments returns the identical answer at four times this.
    assert_eq!(
        deep, one,
        "four segments cost {deep} rounds against {one} for one -- the segments were opened \
         in sequence"
    );
    assert_eq!(
        deep, 2,
        "expected the open round and the legs round, got {deep}"
    );
}

#[tokio::test]
async fn a_multi_segment_query_does_not_list() {
    let (a, b) = split_corpus();
    let t = TenantId(77);
    let acct = Accounted::new(MemoryStore::new());
    let view = acct.as_tenant(t);
    let two = [put(&view, "a", &a).await, put(&view, "b", &b).await];
    query(
        &view,
        &two,
        &text_leg("gamma", 40),
        Fusion::Rrf { k: 60.0 },
        5,
    )
    .await
    .unwrap();
    assert_eq!(
        acct.count(t, OpClass::List),
        0,
        "a multi-segment query listed"
    );
}

#[tokio::test]
async fn a_missing_term_dictionary_is_an_error_not_a_smaller_corpus() {
    // ⚠️ `open` fetches sidecars through a helper that swallows the failure, because a missing
    // CENTROID table legitimately means "scan exactly" (D-10). Carried over to a term
    // dictionary across N segments that becomes silent corruption of a GLOBAL number: the
    // segment drops out of `doc_count` and every `df`, and the whole query is scored against
    // a corpus one segment too small -- confidently, with no error.
    let (a, b) = split_corpus();
    let s = MemoryStore::new();
    let two = [put(&s, "a", &a).await, put(&s, "b", &b).await];
    s.delete_batch(&[text::dict_key(&two[1].segment)])
        .await
        .unwrap();

    let err = query(
        &s,
        &two,
        &text_leg("gamma delta", 40),
        Fusion::Rrf { k: 60.0 },
        5,
    )
    .await
    .expect_err("a segment with postings and no dictionary was scored as absent");
    assert!(
        matches!(err, QueryError::Format(_)),
        "the wrong error kind: {err:?}"
    );
}

#[tokio::test]
async fn a_one_segment_query_is_unchanged() {
    let (a, _) = split_corpus();
    let s = MemoryStore::new();
    let one = [put(&s, "a", &a).await];
    let hits = query(
        &s,
        &one,
        &text_leg("gamma delta", 40),
        Fusion::Rrf { k: 60.0 },
        5,
    )
    .await
    .unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits.iter().all(|h| h.segment == 0),
        "a one-segment query produced a hit from another segment"
    );
}
