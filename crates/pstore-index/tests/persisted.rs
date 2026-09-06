//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The index on the store: round-trip depth, bytes per rung, and the D-10 switch.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section};
use pstore_index::cluster::Params;
use pstore_index::vec_index::{self, EXACT_SCAN_THRESHOLD, Query, Rerank, VecIndex};
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;
use std::sync::Arc;

const DIM: usize = 64;

fn corpus(n: usize, groups: usize, seed: u64) -> Vec<Document> {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let centres: Vec<Vec<f32>> = (0..groups)
        .map(|_| {
            (0..DIM)
                .map(|_| (0..6).map(|_| next()).sum::<f32>() * 1.5)
                .collect()
        })
        .collect();
    (0..n)
        .map(|i| {
            let c = &centres[i % groups];
            let mut v: Vec<f32> = c
                .iter()
                .map(|x| x + (0..6).map(|_| next()).sum::<f32>())
                .collect();
            let m: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            for x in v.iter_mut() {
                *x /= m;
            }
            Document::new(format!("d{i}"), v)
        })
        .collect()
}

/// ⚠️ The threshold is lowered so a fixture of hundreds reaches the clustered path. It is
/// the same switch, exercised at a size the suite can afford: at the real 25,000 these
/// tests took 322 seconds, which `cargo mutants` would pay once per mutant.
/// `the_default_threshold_is_the_stated_number` pins the default itself.
const TEST_THRESHOLD: usize = 200;

fn params() -> Params {
    Params {
        target_list_size: 40,
        exact_scan_threshold: TEST_THRESHOLD,
        ..Params::default()
    }
}

const SEG: &str = "t/idx/seg";
const CEN: &str = "t/idx/centroids";

async fn put<S: BlobStore>(store: &S, docs: &[Document]) -> vec_index::Built {
    let built = vec_index::build(docs, params());
    store
        .put(&Key::new(SEG), built.segment.clone())
        .await
        .unwrap();
    if let Some(c) = &built.centroids {
        store
            .put(&Key::new(CEN), bytes::Bytes::from(c.encode()))
            .await
            .unwrap();
    }
    built
}

#[tokio::test]
async fn a_clustered_query_costs_two_round_trips_beyond_head() {
    // ⚠️ Criterion 6, at the index layer. HEAD is the engine's round; this measures the two
    // that belong to the index -- {footer, centroids} together, then the posting lists --
    // so a cold query from HEAD is three. `rerank: fast` is included in those two, because
    // the sq8 ranges are known at the same moment as the rabitq ranges.
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let docs = corpus(TEST_THRESHOLD + 400, 12, 1);
    put(&*s, &docs).await;

    s.reset();
    let idx = VecIndex::open(&*s, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    assert!(idx.is_clustered());
    assert_eq!(s.depth(), 1, "opening the index took {} rounds", s.depth());

    for rerank in [Rerank::None, Rerank::Fast] {
        s.reset();
        let hits = idx
            .search(
                &*s,
                &Key::new(SEG),
                docs[7].vector(),
                Query {
                    rerank,
                    ..Query::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 10);
        assert_eq!(
            s.depth(),
            1,
            "{rerank:?} took {} rounds for the query itself",
            s.depth()
        );
    }
}

#[tokio::test]
async fn exact_rerank_costs_exactly_one_more_round() {
    // The declared fourth round: which float32 rows to read cannot be chosen until rung 0
    // has ranked, so it is a genuine dependency rather than an oversight.
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let docs = corpus(TEST_THRESHOLD + 400, 12, 2);
    put(&*s, &docs).await;
    let idx = VecIndex::open(&*s, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();

    s.reset();
    idx.search(
        &*s,
        &Key::new(SEG),
        docs[3].vector(),
        Query {
            rerank: Rerank::Exact,
            ..Query::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.depth(), 2, "exact took {} rounds", s.depth());
}

#[tokio::test]
async fn probing_more_lists_costs_bytes_not_depth() {
    // ⚠️ Criterion 7, the lever a memory-resident index does not have: `p` is width.
    // A 256-byte gap: at this fixture's size the default 64 KiB merges every posting
    // list into a single fetch, so `p` appears free in bytes as well as in depth. At
    // production sizing a list is tens of kilobytes and the gap does not reach across one.
    let s = Accounted::new(MemoryStore::with_coalesce_gap(256));
    let t = TenantId(1);
    let v = s.as_tenant(t);
    // ⚠️ Many small lists, so p=8 and p=64 are actually different probe sets. With 15 lists
    // both probe everything and the test passes or fails on the fixture's shape rather than
    // on whether `p` costs bytes.
    let wide = Params {
        target_list_size: 10,
        exact_scan_threshold: TEST_THRESHOLD,
        ..Params::default()
    };
    let docs = corpus(TEST_THRESHOLD + 1_000, 12, 3);
    let built = vec_index::build(&docs, wide);
    v.put(&Key::new(SEG), built.segment.clone()).await.unwrap();
    v.put(
        &Key::new(CEN),
        bytes::Bytes::from(built.centroids.as_ref().unwrap().encode()),
    )
    .await
    .unwrap();
    assert!(
        built.centroids.as_ref().unwrap().vectors.len() > 64,
        "only {} lists: p=8 and p=64 would probe the same set",
        built.centroids.as_ref().unwrap().vectors.len()
    );
    let idx = VecIndex::open(&v, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    let d = Arc::new(DepthCounting::new(MemoryStore::with_coalesce_gap(256)));
    d.put(&Key::new(SEG), built.segment.clone()).await.unwrap();
    d.put(
        &Key::new(CEN),
        bytes::Bytes::from(built.centroids.unwrap().encode()),
    )
    .await
    .unwrap();

    let mut bytes = Vec::new();
    for p in [4usize, 64] {
        let before = s.bytes(t, pstore_blob::OpClass::Read);
        idx.search(
            &v,
            &Key::new(SEG),
            docs[1].vector(),
            Query {
                p,
                ..Query::default()
            },
        )
        .await
        .unwrap();
        bytes.push(s.bytes(t, pstore_blob::OpClass::Read) - before);
    }
    assert!(
        bytes[1] > bytes[0] * 3,
        "p=64 moved {} bytes against p=4's {}: probing wider is not costing bytes, so it is \
         probably not probing wider",
        bytes[1],
        bytes[0]
    );

    // ...and the depth is identical, which is the half that matters.
    let idx2 = VecIndex::open(&*d, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    let mut depths = Vec::new();
    for p in [4usize, 64] {
        d.reset();
        let _ = idx2
            .search(
                &*d,
                &Key::new(SEG),
                docs[1].vector(),
                Query {
                    p,
                    ..Query::default()
                },
            )
            .await;
        depths.push(d.depth());
    }
    assert_eq!(depths[0], depths[1], "p changed the round-trip count");
}

#[tokio::test]
async fn a_rung_zero_query_reads_no_int8_or_float_bytes() {
    // Criterion 4, through the real search path rather than a hand-built fetch.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(2);
    let v = s.as_tenant(t);
    let docs = corpus(TEST_THRESHOLD + 400, 12, 4);
    put(&v, &docs).await;
    let idx = VecIndex::open(&v, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    let vectors = idx.segment().section(Section::Vectors).unwrap();
    let eights = idx.segment().section(Section::Sq8).unwrap();

    s.record_ranges();
    idx.search(
        &v,
        &Key::new(SEG),
        docs[5].vector(),
        Query {
            rerank: Rerank::None,
            ..Query::default()
        },
    )
    .await
    .unwrap();
    let key = Key::new(SEG);
    assert_eq!(
        s.bytes_in(&key, vectors),
        0,
        "rung 0 read full-precision bytes"
    );
    assert_eq!(s.bytes_in(&key, eights), 0, "rung 0 read int8 bytes");
    assert!(s.bytes_in(&key, idx.segment().section(Section::RaBitQ).unwrap()) > 0);
}

#[tokio::test]
async fn a_small_index_answers_exactly_and_builds_no_index() {
    // ⚠️ Criterion 11, both halves. Below the threshold there is no centroid object at all
    // -- not an empty one -- so "is this clustered?" is answered by whether the object
    // exists, and a query is exactly right rather than approximately.
    let s = MemoryStore::new();
    let docs = corpus(TEST_THRESHOLD - 50, 10, 5);
    let built = put(&s, &docs).await;
    assert!(built.centroids.is_none(), "a small index built centroids");
    assert!(
        s.get(&Key::new(CEN)).await.is_err(),
        "a centroid object exists for an index that should be scanned exactly"
    );

    let idx = VecIndex::open(&s, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    assert!(!idx.is_clustered());

    // Exactly right, not approximately: the top-10 must match brute force.
    let query = docs[42].vector();
    let mut want: Vec<(usize, f32)> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            (
                i,
                d.vector()
                    .iter()
                    .zip(query.iter())
                    .map(|(a, b)| a * b)
                    .sum(),
            )
        })
        .collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let got = idx
        .search(&s, &Key::new(SEG), query, Query::default())
        .await
        .unwrap();
    let got_ids: Vec<usize> = got.iter().map(|(r, _)| built.order[*r]).collect();
    let want_ids: Vec<usize> = want.iter().take(10).map(|(i, _)| *i).collect();
    assert_eq!(got_ids, want_ids, "an exactly-scanned index was not exact");
}

#[tokio::test]
async fn the_threshold_is_the_only_thing_that_switches_paths() {
    // One row either side. A second switch anywhere -- a size check, a fallback on an empty
    // centroid table -- would let a broken clustered path hide behind the exact one.
    let s = MemoryStore::new();
    let below = vec_index::build(&corpus(TEST_THRESHOLD - 1, 12, 6), params());
    let above = vec_index::build(&corpus(TEST_THRESHOLD, 12, 6), params());
    assert!(
        below.centroids.is_none(),
        "one row below the threshold clustered"
    );
    assert!(
        above.centroids.is_some(),
        "one row at the threshold did not cluster"
    );
    let _ = s;
}

#[tokio::test]
async fn the_default_threshold_is_the_stated_number() {
    // The tests above lower it to stay fast. This is what stops that from quietly becoming
    // the real value -- the switch is tested small, the number is pinned here.
    assert_eq!(EXACT_SCAN_THRESHOLD, 25_000);
    assert_eq!(Params::default().exact_scan_threshold, EXACT_SCAN_THRESHOLD);
}

#[tokio::test]
async fn a_corrupt_centroid_table_is_refused_not_guessed() {
    // A short table would otherwise yield centroids of the wrong dimension and spans
    // pointing at the wrong rows: a wrong answer rather than an error.
    let s = MemoryStore::new();
    let docs = corpus(TEST_THRESHOLD + 400, 12, 7);
    put(&s, &docs).await;
    s.put(&Key::new(CEN), bytes::Bytes::from_static(b"xx"))
        .await
        .unwrap();
    let idx = VecIndex::open(&s, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    // Falls back to the exact path rather than decoding garbage.
    assert!(!idx.is_clustered());
}

#[tokio::test]
async fn the_rerank_knob_buys_recall_and_not_only_bytes() {
    // ⚠️ Criterion 8, and the gap that let a mutation live: the depth tests above check
    // what each rung COSTS, and the rung-0 byte test uses `None`. Nothing checked that
    // `Fast` returns anything useful -- so a search that fetched no int8 at all and scored
    // every survivor as negative infinity passed the whole file.
    let s = MemoryStore::new();
    let docs = corpus(TEST_THRESHOLD + 400, 12, 11);
    put(&s, &docs).await;
    let idx = VecIndex::open(&s, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    let built = vec_index::build(&docs, params());

    let mut hits = [0usize; 3];
    for qi in [1usize, 40, 90, 150, 220] {
        let query = docs[qi].vector();
        let mut want: Vec<(usize, f32)> = docs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                (
                    i,
                    d.vector()
                        .iter()
                        .zip(query.iter())
                        .map(|(a, b)| a * b)
                        .sum(),
                )
            })
            .collect();
        want.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let want: Vec<usize> = want.iter().take(10).map(|(i, _)| *i).collect();

        for (n, rerank) in [Rerank::None, Rerank::Fast, Rerank::Exact]
            .into_iter()
            .enumerate()
        {
            let got = idx
                .search(
                    &s,
                    &Key::new(SEG),
                    query,
                    Query {
                        rerank,
                        ..Query::default()
                    },
                )
                .await
                .unwrap();
            let ids: Vec<usize> = got.iter().map(|(r, _)| built.order[*r]).collect();
            hits[n] += want.iter().filter(|w| ids.contains(w)).count();
        }
    }
    assert!(
        hits[1] > hits[0],
        "`fast` found {} of the true neighbours against `none`'s {}: the int8 rung is \
         costing a section of every segment and buying nothing",
        hits[1],
        hits[0]
    );
    assert!(
        hits[2] >= hits[1],
        "`exact` found {} against `fast`'s {}",
        hits[2],
        hits[1]
    );
    assert!(hits[2] >= 45, "exact rerank found only {} of 50", hits[2]);
}

#[tokio::test]
async fn a_cold_query_from_head_costs_three_round_trips() {
    // ⚠️ Criterion 6, composed. The layer that joins the engine to the index is
    // `pstore-query` (layer 4) and does not exist yet, so this test does the joining --
    // legitimately, because `pstore-index` is layer 3 and may depend DOWNWARD on
    // `pstore-engine`. What it measures is the real sequence a client experiences:
    //
    //   1. read HEAD, which names the segment
    //   2. the segment footer and the centroid object, together -- different objects, both
    //      keys derived, so width rather than depth
    //   3. the probed posting lists, rabitq and sq8 ranges in one call
    //
    // Three. `rerank: fast` is inside them; only `exact` adds a fourth, and it says so.
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let t = TenantId(77);
    let docs = corpus(TEST_THRESHOLD + 400, 12, 21);
    let built = vec_index::build(&docs, params());
    s.put(&Key::new(SEG), built.segment.clone()).await.unwrap();
    s.put(
        &Key::new(CEN),
        bytes::Bytes::from(built.centroids.as_ref().unwrap().encode()),
    )
    .await
    .unwrap();

    // A HEAD naming that segment, committed the way a fold would.
    let engine = pstore_engine::Engine::new(Arc::clone(&s), t, pstore_types::LaneId(0));
    engine
        .commit_head_for_test(|h| {
            h.indexes.insert(
                "idx".to_owned(),
                vec![pstore_engine::SegmentRef {
                    key: SEG.to_owned(),
                    rows: docs.len() as u32,
                }],
            );
        })
        .await
        .unwrap();

    // Cold: nothing cached, starting from the tenant id alone.
    s.reset();
    let head = engine.head_for_test().await;
    let seg_key = Key::new(head.indexes["idx"][0].key.clone());
    let idx = VecIndex::open(&*s, &seg_key, &Key::new(CEN), DIM)
        .await
        .unwrap();
    let hits = idx
        .search(&*s, &seg_key, docs[9].vector(), Query::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 10);
    assert_eq!(
        s.depth(),
        3,
        "a cold query from HEAD took {} sequential round trips",
        s.depth()
    );
}

#[tokio::test]
async fn a_rerank_rung_reads_only_the_rows_it_scores() {
    // ⚠️ The ceiling was only ever asserted at the DEFAULT mode, which is how `exact`
    // shipped reading the ENTIRE full-precision section -- every row in the segment -- to
    // score the 320 survivors rung 0 handed it. Sixty times the bytes, past the query byte
    // ceiling, and invisible: the depth tests measure rounds, and the rung-0 byte test uses
    // `none`.
    //
    // Asserted against rows SCORED rather than against an absolute, so it says what is
    // wrong rather than encoding this fixture's size.
    // ⚠️ A 256-byte coalescing gap. At this fixture's size the survivors are ~960 bytes
    // apart, well inside the default 64 KiB, so the coalescer merges them into one fetch
    // spanning the whole section -- correctly, because a 960-byte hole is cheaper than a
    // second request, and the fix then looks like it changed nothing. At gate scale the
    // spacing is ~96 KB and they do not merge, which is where the 60x actually lives.
    let s = Accounted::new(MemoryStore::with_coalesce_gap(256));
    let t = TenantId(9);
    let v = s.as_tenant(t);
    let docs = corpus(TEST_THRESHOLD + 1_000, 12, 31);
    put(&v, &docs).await;
    let idx = VecIndex::open(&v, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    let vectors = idx.segment().section(Section::Vectors).unwrap();
    let q = Query::default();
    // Rung 2 scores the survivors rung 0 kept: k x oversample.
    let survivors = q.k * q.oversample;

    s.record_ranges();
    idx.search(
        &v,
        &Key::new(SEG),
        docs[11].vector(),
        Query {
            rerank: Rerank::Exact,
            ..q
        },
    )
    .await
    .unwrap();
    let read = s.bytes_in(&Key::new(SEG), vectors.clone());
    // x4 slack for coalescing, which legitimately fetches the holes between scattered rows
    // when they are cheaper than another request.
    let budget = (survivors * DIM * 4 * 4) as u64;
    assert!(
        read <= budget,
        "exact rerank read {read} full-precision bytes to score {survivors} rows, against a \
         budget of {budget}: it is fetching rows it never looks at"
    );
    assert!(read > 0, "exact rerank read no full-precision bytes at all");
}

#[tokio::test]
async fn a_query_names_the_field_it_searches() {
    // ⚠️ M3b.4. With named fields, "search this index" is ambiguous — a document may carry a
    // body embedding and a title embedding of different dimensions. A query that does not
    // name its field either guesses or searches whichever happens to be first, and both are
    // wrong answers rather than errors.
    let s = MemoryStore::new();
    let docs: Vec<Document> = (0..TEST_THRESHOLD + 400)
        .map(|i| {
            let base = corpus(1, 1, i as u64 + 1)[0].clone();
            Document {
                id: format!("d{i}"),
                vectors: std::collections::BTreeMap::from([(
                    "body".to_owned(),
                    pstore_format::VectorField::dense(base.vector().to_vec()),
                )]),
                attrs: Default::default(),
            }
        })
        .collect();
    let built = vec_index::build_field(&docs, params(), "body");
    s.put(&Key::new(SEG), built.segment.clone()).await.unwrap();
    s.put(
        &Key::new(CEN),
        bytes::Bytes::from(built.centroids.as_ref().unwrap().encode()),
    )
    .await
    .unwrap();

    let idx = VecIndex::open(&s, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();
    let q = &docs[9].field("body")[0];

    // The field the index was built for answers.
    let hits = idx
        .search_field(&s, &Key::new(SEG), "body", q, Query::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 10);

    // A field the segment does not carry is an error, not an empty result: a caller cannot
    // otherwise tell a typo from a legitimately unpopulated field.
    assert!(
        idx.search_field(&s, &Key::new(SEG), "title", q, Query::default())
            .await
            .is_err(),
        "searching an absent field returned a result set"
    );
}

#[tokio::test]
async fn a_ragged_dimension_does_not_shift_every_later_row() {
    // ⚠️ Labelled "unreachable" in `build_field` and it is not: `dim` comes from the FIRST
    // document that has the field, so a later document with a different width fails to
    // encode. Codes are fixed-stride, so dropping that row would shift every row after it
    // and every subsequent search would return the wrong documents — plausible ones, with
    // no error anywhere.
    let s = MemoryStore::new();
    let mut docs = corpus(TEST_THRESHOLD + 400, 12, 41);
    // One row of the wrong width, in the middle.
    let bad = docs.len() / 2;
    docs[bad] = Document {
        id: format!("d{bad}"),
        vectors: std::collections::BTreeMap::from([(
            pstore_format::DEFAULT_FIELD.to_owned(),
            pstore_format::VectorField::dense(vec![0.5; DIM * 2]),
        )]),
        attrs: Default::default(),
    };
    let built = vec_index::build(&docs, params());
    s.put(&Key::new(SEG), built.segment.clone()).await.unwrap();
    s.put(
        &Key::new(CEN),
        bytes::Bytes::from(built.centroids.as_ref().unwrap().encode()),
    )
    .await
    .unwrap();
    let idx = VecIndex::open(&s, &Key::new(SEG), &Key::new(CEN), DIM)
        .await
        .unwrap();

    // A query for a known-good document still finds it: the stride survived.
    let target = 7usize;
    let hits = idx
        .search(&s, &Key::new(SEG), docs[target].vector(), Query::default())
        .await
        .unwrap();
    let ids: Vec<usize> = hits.iter().map(|(r, _)| built.order[*r]).collect();
    assert!(
        ids.contains(&target),
        "one ragged row shifted the stride: searching for d{target} returned {ids:?}"
    );
}
