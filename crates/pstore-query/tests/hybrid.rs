//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Two retrievers over one segment.
//!
//! ⚠️ **Every claim here is invisible to a functional test.** Legs awaited in sequence return
//! the same rows as legs joined. A segment opened twice returns the same rows as a segment
//! opened once. A dropped retriever returns a plausible ranking. What separates right from
//! wrong is the request count and the depth, so that is what is asserted.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{DEFAULT_FIELD, Document, Impact, Value, VectorField, sparse, text};
use pstore_index::cluster::Params;
use pstore_index::vec_index;
use pstore_query::{Fusion, Prefetch, QueryError, Target, query};
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;

const SEG: &str = "t/idx/hybrid.seg";
const CEN: &str = "t/idx/hybrid.centroids";
const SPARSE: &str = "body_sparse";
const DIM: usize = 8;
/// ⚠️ Pinned: at the 64 KiB default the coalescer merges a fixture's whole section, and a
/// request count measured there is a property of the fixture rather than of the code.
const GAP: u64 = 256;

/// One segment, as the query now takes them.
///
/// ⚠️ Paired rather than two arguments: a centroid table belongs to one segment, and a shared
/// one would give every segment's dense leg another segment's clusters.
fn one(segment: &Key, centroids: &Key) -> [Target; 1] {
    [Target {
        segment: segment.clone(),
        centroids: centroids.clone(),
        deleted: None,
        shadowed: false,
    }]
}

fn hybrid_doc(i: usize) -> Document {
    let mut d = Document::new(
        format!("d{i:05}"),
        (0..DIM).map(|j| ((i + j) % 11) as f32 / 11.0).collect(),
    );
    // ⚠️ The sparse field sorts BEFORE the dense one, which is the case that would have taken
    // the legacy section ids from the dense field and made this whole leg return nothing.
    d.vectors.insert(
        SPARSE.to_owned(),
        VectorField::Sparse(vec![
            (i as u32 % 23, Impact::new(0.9)),
            (50 + (i as u32 % 7), Impact::new(0.4)),
        ]),
    );
    d.attrs.insert(
        text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str(format!("document {i} quarterly revenue w{}", i % 29)),
    );
    d
}

fn params() -> Params {
    Params {
        target_list_size: 40,
        exact_scan_threshold: 200,
        ..Params::default()
    }
}

async fn put<S: BlobStore>(store: &S, docs: &[Document]) -> (Key, Key) {
    let built = vec_index::build_hybrid(docs, params(), DEFAULT_FIELD, Some(SPARSE));
    let (seg, cen) = (Key::new(SEG), Key::new(CEN));
    store.put(&seg, built.segment).await.unwrap();
    if let Some(c) = &built.centroids {
        store
            .put(&cen, bytes::Bytes::from(c.encode()))
            .await
            .unwrap();
    }
    store
        .put(
            &sparse::dict_key(&seg),
            bytes::Bytes::from(built.dictionary.expect("no dictionary was built")),
        )
        .await
        .unwrap();
    (seg, cen)
}

/// The same segment, plus a text index over the `text` attribute.
///
/// ⚠️ Built by **one** builder. Three indexes assembled separately over the input order are
/// three internally consistent structures pointing at three different documents, because the
/// dense clustering is what decides the segment's row order.
async fn put_with_text<S: BlobStore>(store: &S, docs: &[Document]) -> (Key, Key) {
    let built = vec_index::build_all(
        docs,
        params(),
        DEFAULT_FIELD,
        Some(SPARSE),
        Some(text::DEFAULT_TEXT_FIELD),
    );
    let (seg, cen) = (Key::new(SEG), Key::new(CEN));
    store.put(&seg, built.segment).await.unwrap();
    if let Some(c) = &built.centroids {
        store
            .put(&cen, bytes::Bytes::from(c.encode()))
            .await
            .unwrap();
    }
    store
        .put(
            &sparse::dict_key(&seg),
            bytes::Bytes::from(built.dictionary.expect("no sparse dictionary")),
        )
        .await
        .unwrap();
    store
        .put(
            &text::dict_key(&seg),
            bytes::Bytes::from(built.text_dictionary.expect("no term dictionary")),
        )
        .await
        .unwrap();
    (seg, cen)
}

fn legs(docs: &[Document]) -> Vec<Prefetch> {
    vec![
        Prefetch::Dense {
            field: DEFAULT_FIELD.to_owned(),
            query: docs[7].field(DEFAULT_FIELD)[0].clone(),
            limit: 20,
            tune: vec_index::Query::default(),
        },
        Prefetch::Sparse {
            field: SPARSE.to_owned(),
            query: vec![(7, 1.0), (50, 0.5)],
            limit: 20,
        },
    ]
}

fn corpus(n: usize) -> Vec<Document> {
    (0..n).map(hybrid_doc).collect()
}

#[tokio::test]
async fn a_hybrid_query_opens_the_segment_once() {
    // ⚠️ Criterion 4, and it needs the UNCACHED stack to mean anything: `pstore-cache`
    // singleflights identical concurrent reads, so with a cache in the stack two legs each
    // calling `Segment::open` bill exactly one read — the test passing for code that opens
    // twice.
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let t = TenantId(1);
    let view = acct.as_tenant(t);
    let docs = corpus(600);
    let (seg, cen) = put(&view, &docs).await;

    // The segment's length, so the footer read is identifiable: `Segment::open` is the only
    // read whose span ENDS at the end of the object -- every leg's ranges sit in the data
    // area, before the meta region.
    let len = view.head(&seg).await.unwrap();
    acct.record_ranges();
    let hits = query(&view, &one(&seg, &cen), &legs(&docs), Fusion::default(), 10)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "the fixture produced no hits at all");

    let opens = acct
        .ranges()
        .iter()
        .filter(|(k, r)| *k == seg && r.end == len)
        .count();
    assert_eq!(
        opens, 1,
        "a two-leg query opened the segment {opens} times; each leg opening for itself is \
         one extra suffix read and one extra Meta admission per retriever"
    );
    assert_eq!(acct.count(t, OpClass::List), 0, "a hybrid query listed");
}

#[tokio::test]
async fn a_hybrid_query_is_no_deeper_than_its_deepest_leg() {
    // ⚠️ Criterion 5, the whole point of `prefetch[]`. Two legs awaited in sequence give the
    // same ranking at four rounds instead of two, and nothing functional can tell.
    let inner = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put(&inner, &docs).await;
    let s = DepthCounting::new(inner);

    let l = legs(&docs);
    s.reset();
    query(&s, &one(&seg, &cen), &l, Fusion::default(), 10)
        .await
        .unwrap();
    let both = s.depth();

    s.reset();
    query(&s, &one(&seg, &cen), &l[..1], Fusion::default(), 10)
        .await
        .unwrap();
    let dense_only = s.depth();

    s.reset();
    query(&s, &one(&seg, &cen), &l[1..], Fusion::default(), 10)
        .await
        .unwrap();
    let sparse_only = s.depth();

    assert_eq!(
        both,
        dense_only.max(sparse_only),
        "two legs cost {both} rounds against {dense_only} and {sparse_only} alone: the sum, \
         not the max"
    );
    assert_eq!(both, 2, "a hybrid query cost {both} rounds beyond HEAD");
}

#[tokio::test]
async fn an_unimplemented_retriever_is_refused() {
    // ⚠️ Criterion 6. D-73 ships the SHAPE before the retriever, so the shape has to say no.
    // An arm falling through to `Ok(vec![])` answers with a plausible ranking computed from
    // fewer retrievers than were asked for, and nothing anywhere says so.
    let store = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put(&store, &docs).await;
    // ⚠️ The occupant has changed and the mechanism has not. In M5b this was `Text`; M5c
    // implemented it, so the shape's unimplemented retriever is now `Trigram`, which
    // `full-text-search.md` names as "the same inverted machinery". D-73's point is that the
    // shape carries retrievers that do not exist yet — so as long as one does not, this test
    // has something to be about.
    let mut l = legs(&docs);
    l.push(Prefetch::Trigram {
        field: "body".to_owned(),
        pattern: "quarterly.*revenue".to_owned(),
        limit: 20,
    });
    match query(&store, &one(&seg, &cen), &l, Fusion::default(), 10).await {
        Err(QueryError::Unimplemented(which)) => assert_eq!(which, "trigram"),
        other => panic!("a trigram prefetch was not refused by name: {other:?}"),
    }
}

#[tokio::test]
async fn a_refused_retriever_costs_no_requests() {
    // Refused BEFORE any I/O. A request naming a retriever we cannot run is not a request we
    // half-run and then abandon — the caller is billed for the reads either way.
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let t = TenantId(2);
    let view = acct.as_tenant(t);
    let docs = corpus(600);
    let (seg, cen) = put(&view, &docs).await;
    let before = acct.count(t, OpClass::Read);
    let _ = query(
        &view,
        &one(&seg, &cen),
        &[Prefetch::Trigram {
            field: "body".to_owned(),
            pattern: "x".to_owned(),
            limit: 5,
        }],
        Fusion::default(),
        10,
    )
    .await;
    assert_eq!(
        acct.count(t, OpClass::Read),
        before,
        "a refused query still went to the store"
    );
}

#[tokio::test]
async fn an_empty_leg_is_not_an_error() {
    // ⚠️ Criterion 7. A retriever that matches nothing is a normal outcome, not a failure —
    // and a `?` on it turns half a hybrid answer into no answer at all.
    let store = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put(&store, &docs).await;

    let mut l = legs(&docs);
    l[1] = Prefetch::Sparse {
        field: SPARSE.to_owned(),
        query: vec![(900_000, 1.0)],
        limit: 20,
    };
    let hits = query(&store, &one(&seg, &cen), &l, Fusion::default(), 10)
        .await
        .expect("a leg that matched nothing was an error");
    assert!(!hits.is_empty(), "the dense leg's hits were lost with it");

    let none = query(
        &store,
        &one(&seg, &cen),
        &[Prefetch::Sparse {
            field: SPARSE.to_owned(),
            query: vec![(900_000, 1.0)],
            limit: 20,
        }],
        Fusion::default(),
        10,
    )
    .await
    .expect("a query whose every leg matched nothing was an error");
    assert!(none.is_empty());
}

#[tokio::test]
async fn a_legs_limit_bounds_only_that_leg() {
    // ⚠️ Criterion 8. `limit` applied after fusion instead of per leg lets one retriever's
    // rows crowd out the other's before fusion ever sees them — which is the failure mode
    // `prefetch[]` exists to make impossible.
    let store = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put(&store, &docs).await;

    let mut l = legs(&docs);
    if let Prefetch::Sparse { limit, .. } = &mut l[1] {
        *limit = 2;
    }
    let hits = query(&store, &one(&seg, &cen), &l, Fusion::default(), 50)
        .await
        .unwrap();
    let wide = query(
        &store,
        &one(&seg, &cen),
        &legs(&docs),
        Fusion::default(),
        50,
    )
    .await
    .unwrap();
    assert!(
        hits.len() < wide.len(),
        "narrowing one leg to 2 did not narrow the fused answer ({} against {})",
        hits.len(),
        wide.len()
    );
    assert!(
        hits.len() >= 20,
        "narrowing the SPARSE leg to 2 also narrowed the dense one: {} rows",
        hits.len()
    );
}

#[tokio::test]
async fn a_dense_query_is_unaffected_by_the_sparse_field_beside_it() {
    // ⚠️ M5a criterion 4, exercised where it actually bites. `body_sparse` sorts before
    // `vector`, and field 0 keeps the legacy section ids: without the dense-only field index
    // this returns ZERO rows and the hybrid answer is the sparse leg wearing both names.
    let store = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put(&store, &docs).await;
    let dense_only: Vec<Document> = docs
        .iter()
        .map(|d| Document::new(d.id.clone(), d.field(DEFAULT_FIELD)[0].clone()))
        .collect();

    let plain = vec_index::build(&dense_only, params());
    let store2 = MemoryStore::with_coalesce_gap(GAP);
    let (s2, c2) = (Key::new(SEG), Key::new(CEN));
    store2.put(&s2, plain.segment).await.unwrap();
    store2
        .put(
            &c2,
            bytes::Bytes::from(plain.centroids.as_ref().unwrap().encode()),
        )
        .await
        .unwrap();

    let leg = &legs(&docs)[..1];
    let with_sparse = query(&store, &one(&seg, &cen), leg, Fusion::default(), 10)
        .await
        .unwrap();
    let without = query(&store2, &one(&s2, &c2), leg, Fusion::default(), 10)
        .await
        .unwrap();
    assert!(!with_sparse.is_empty(), "the dense leg returned nothing");
    assert_eq!(
        with_sparse.iter().map(|h| h.row).collect::<Vec<_>>(),
        without.iter().map(|h| h.row).collect::<Vec<_>>(),
        "a sparse field beside the dense one changed the dense answer"
    );
}

#[tokio::test]
async fn a_three_leg_query_is_no_deeper_than_its_deepest_leg() {
    // ⚠️ M5c criterion 11. Three retrievers, one open, one round of ranges. The failure this
    // catches returns the same ranking at three times the depth, and no functional assertion
    // can tell.
    let inner = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put_with_text(&inner, &docs).await;
    let s = DepthCounting::new(inner);

    let mut l = legs(&docs);
    l.push(Prefetch::Text {
        field: text::DEFAULT_TEXT_FIELD.to_owned(),
        query: "quarterly revenue".to_owned(),
        limit: 20,
    });

    s.reset();
    let three = query(&s, &one(&seg, &cen), &l, Fusion::default(), 10)
        .await
        .unwrap();
    let depth_three = s.depth();
    assert!(!three.is_empty(), "the three-leg query returned nothing");

    s.reset();
    query(&s, &one(&seg, &cen), &l[2..], Fusion::default(), 10)
        .await
        .unwrap();
    let text_only = s.depth();

    assert_eq!(
        depth_three, text_only,
        "three legs cost {depth_three} rounds against {text_only} for the text leg alone"
    );
    assert_eq!(
        depth_three, 2,
        "a three-leg query cost {depth_three} rounds"
    );
}

#[tokio::test]
async fn a_text_leg_is_no_longer_refused_and_still_names_what_is_missing() {
    // The D-73 shape is unchanged: `query` is still a `String`, analyzed at query time. What
    // changed is that it runs.
    let store = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put_with_text(&store, &docs).await;
    let hits = query(
        &store,
        &one(&seg, &cen),
        &[Prefetch::Text {
            field: text::DEFAULT_TEXT_FIELD.to_owned(),
            query: "Quarterly, REVENUE".to_owned(),
            limit: 10,
        }],
        Fusion::default(),
        10,
    )
    .await
    .expect("a text leg was refused");
    assert!(!hits.is_empty(), "the analyzer did not lowercase the query");

    // A segment with no term dictionary is an error naming the sidecar, not an empty answer.
    let (bare, bare_cen) = put(&store, &docs).await;
    assert!(
        query(
            &store,
            &one(&bare, &bare_cen),
            &[Prefetch::Text {
                field: text::DEFAULT_TEXT_FIELD.to_owned(),
                query: "revenue".to_owned(),
                limit: 10,
            }],
            Fusion::default(),
            10,
        )
        .await
        .is_err(),
        "a text leg over a segment with no text answered instead of failing"
    );
}

#[tokio::test]
async fn a_dense_and_a_sparse_query_are_unaffected_by_a_text_field() {
    // ⚠️ M5c criterion 3, and the reason a text field has no `Fields` row. A row for it would
    // send its postings to `decode_field`, which reads them as f32 -- so the dense field's
    // section ids, the `simple` fast path in `scan`, and every field the reader enumerates
    // would all shift. The failure is silent: a dense query over a segment that also carries
    // text would return zero rows or the wrong ones, and only a comparison against the same
    // corpus WITHOUT text can see it.
    let docs = corpus(600);
    let with_text = MemoryStore::with_coalesce_gap(GAP);
    let (seg_t, cen_t) = put_with_text(&with_text, &docs).await;
    let without = MemoryStore::with_coalesce_gap(GAP);
    let (seg_p, cen_p) = put(&without, &docs).await;

    for (n, leg) in legs(&docs).into_iter().enumerate() {
        let only = std::slice::from_ref(&leg);
        let a = query(
            &with_text,
            &one(&seg_t, &cen_t),
            only,
            Fusion::default(),
            10,
        )
        .await
        .unwrap();
        let b = query(&without, &one(&seg_p, &cen_p), only, Fusion::default(), 10)
            .await
            .unwrap();
        assert!(
            !a.is_empty(),
            "leg {n} returned nothing beside a text field"
        );
        assert_eq!(
            a.iter().map(|h| h.row).collect::<Vec<_>>(),
            b.iter().map(|h| h.row).collect::<Vec<_>>(),
            "leg {n}'s rows changed when a text field shared the segment"
        );
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.score, y.score, "leg {n}'s scores changed");
        }
    }
}

#[tokio::test]
async fn a_text_leg_naming_another_field_is_refused() {
    // ⚠️ D-73's failure at field granularity. A segment carries one text field, so a request
    // naming a different one would be answered with `text`'s ranking and nothing anywhere
    // would say so -- a plausible answer to a question nobody asked. Both sibling legs already
    // refuse an unknown field name; this one used to drop it.
    let store = MemoryStore::with_coalesce_gap(GAP);
    let docs = corpus(600);
    let (seg, cen) = put_with_text(&store, &docs).await;
    let wrong = query(
        &store,
        &one(&seg, &cen),
        &[Prefetch::Text {
            field: "body".to_owned(),
            query: "revenue".to_owned(),
            limit: 10,
        }],
        Fusion::default(),
        10,
    )
    .await;
    assert!(
        wrong.is_err(),
        "a text leg naming a field the segment does not have returned {} hits",
        wrong.map(|h| h.len()).unwrap_or(0)
    );
    // The field it does have still works.
    assert!(
        !query(
            &store,
            &one(&seg, &cen),
            &[Prefetch::Text {
                field: text::DEFAULT_TEXT_FIELD.to_owned(),
                query: "revenue".to_owned(),
                limit: 10,
            }],
            Fusion::default(),
            10,
        )
        .await
        .unwrap()
        .is_empty()
    );
}
