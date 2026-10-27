//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Several text fields in one segment (M151): a query over one of them answers exactly as a
//! segment built over that field alone.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::text::{self, Analyzer, FullText};
use pstore_format::{Document, FormatError, Section, Segment, Value};
use pstore_index::cluster::Params;
use pstore_index::vec_index;
use pstore_query::{
    Elsewhere, Fusion, Hit, Prefetch, QueryError, Share, Target, Weights, open_part,
    query_rows_split, query_with, scan_part,
};
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;

const DIM: usize = 8;
const FIELDS: [&str; 3] = ["a", "b", "c"];

fn analyzer(field: &str) -> Analyzer {
    match field {
        "a" => Analyzer::default(),
        "b" => Analyzer {
            stemming: true,
            remove_stopwords: true,
            ..Analyzer::default()
        },
        _ => Analyzer {
            case_sensitive: true,
            ..Analyzer::default()
        },
    }
}

fn fts(field: &str) -> FullText {
    FullText {
        analyzer: analyzer(field),
        ..FullText::default()
    }
}

const WORDS: [&str; 9] = [
    "quarterly",
    "revenue",
    "Revenue",
    "the",
    "running",
    "runs",
    "growth",
    "margin",
    "fox",
];

/// Rows over `a`, `b` and `c`; `with_c` false leaves `c` without text in every row.
fn rows(base: usize, with_c: bool) -> Vec<Document> {
    (base..base + 60)
        .map(|i| {
            let mut d = Document::new(
                format!("d{i:05}"),
                (0..DIM).map(|j| ((i + j) % 11) as f32 / 11.0).collect(),
            );
            let w = |k: usize| WORDS[(i * k + k / 2) % WORDS.len()];
            let s = |x: String| Value::Str(x);
            if i % 4 != 0 {
                d.attrs
                    .insert("a".into(), s(format!("{} {} {}", w(1), w(2), w(3))));
            }
            if i % 3 != 0 {
                d.attrs.insert(
                    "b".into(),
                    Value::Array(vec![s(format!("{} {}", w(5), w(7))), s(w(11).into())]),
                );
            }
            if i % 2 == 0 {
                d.attrs.insert(
                    "c".into(),
                    s(if with_c {
                        format!("{} {}", w(13), w(2))
                    } else {
                        String::new()
                    }),
                );
            }
            d
        })
        .collect()
}

fn params() -> Params {
    Params {
        exact_scan_threshold: 10_000,
        ..Params::default()
    }
}

/// One segment over `docs` with text fields `fields`, and its dictionary beside it.
async fn put<S: BlobStore>(store: &S, name: &str, docs: &[Document], fields: &[&str]) -> Target {
    let analyzers: Vec<Analyzer> = fields.iter().map(|f| analyzer(f)).collect();
    let text: Vec<(&str, &Analyzer)> = fields.iter().copied().zip(&analyzers).collect();
    let built = vec_index::try_build_all_fields(
        docs,
        params(),
        pstore_format::DEFAULT_FIELD,
        None,
        &text,
        &[],
    )
    .unwrap();
    let key = Key::new(format!("t/idx/{name}.seg"));
    store.put(&key, built.segment).await.unwrap();
    let has_text = built.text_dictionary.is_some();
    if let Some(d) = built.text_dictionary {
        store
            .put(&text::dict_key(&key), bytes::Bytes::from(d))
            .await
            .unwrap();
    }
    Target {
        segment_len: None,
        centroids: None,
        deleted: None,
        sparse_dict: false,
        text_dict: has_text,
        fde_clusters: false,
        shadowed: false,
        segment: key,
    }
}

fn text_leg(field: &str, q: &str) -> Prefetch {
    Prefetch::Text {
        field: field.to_owned(),
        query: q.to_owned(),
        limit: 40,
    }
}

const QUERIES: [&str; 5] = [
    "quarterly revenue",
    "Revenue growth",
    "running fox",
    "the margin",
    "runs",
];

/// The three shapes criterion 3 names, over `field`.
fn shapes(field: &str, q: &str, docs: &[Document]) -> Vec<(Vec<Prefetch>, Fusion)> {
    let sum = |n: usize| Fusion::Sum {
        weights: Weights::of(&vec![1.0; n]).unwrap(),
    };
    vec![
        (vec![text_leg(field, q)], sum(1)),
        (
            vec![text_leg(field, q), text_leg(field, "margin growth")],
            sum(2),
        ),
        (
            vec![
                Prefetch::Dense {
                    field: pstore_format::DEFAULT_FIELD.to_owned(),
                    query: docs[7].field(pstore_format::DEFAULT_FIELD)[0].clone(),
                    limit: 20,
                    tune: vec_index::Query::default(),
                },
                text_leg(field, q),
            ],
            Fusion::Rrf { k: 60.0 },
        ),
    ]
}

fn same(got: &[Hit], want: &[Hit], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: {got:?} against {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert_eq!((g.segment, g.row), (w.segment, w.row), "{what}");
        assert!(
            (g.score - w.score).abs() <= 1e-6,
            "{what}: {g:?} against {w:?}"
        );
    }
}

#[tokio::test]
async fn one_text_field_writes_the_segment_it_always_wrote() {
    // M151 criterion 1, the segment's half.
    let docs = rows(0, true);
    for f in FIELDS {
        let a = analyzer(f);
        let new = vec_index::try_build_all_fields(
            &docs,
            params(),
            pstore_format::DEFAULT_FIELD,
            None,
            &[(f, &a)],
            &[],
        )
        .unwrap();
        let old = vec_index::try_build_all_with(
            &docs,
            params(),
            pstore_format::DEFAULT_FIELD,
            None,
            Some(f),
            &a,
            &[],
        )
        .unwrap();
        assert_eq!(new.segment, old.segment, "{f}");
        assert_eq!(new.text_dictionary, old.text_dictionary, "{f}");
        assert!(new.text_dictionary.is_some(), "{f} has no text to compare");
    }
}

#[tokio::test]
async fn a_field_of_several_answers_as_its_own_segment_would() {
    // M151 criterion 3.
    let s = MemoryStore::new();
    let (one, two) = (rows(0, true), rows(60, false));
    let many = [
        put(&s, "m0", &one, &FIELDS).await,
        put(&s, "m1", &two, &FIELDS).await,
    ];
    let carried = Segment::open(&s, &many[1].segment).await.unwrap();
    assert_eq!(
        carried.text_fields(),
        ["a", "b"],
        "c was carried without text"
    );
    let mut answered = 0;
    for f in FIELDS {
        let alone = [
            put(&s, &format!("{f}0"), &one, &[f]).await,
            put(&s, &format!("{f}1"), &two, &[f]).await,
        ];
        for q in QUERIES {
            for (legs, fusion) in shapes(f, q, &one) {
                let got = query_with(&s, &many, &legs, &fts(f), fusion, 20)
                    .await
                    .unwrap();
                let want = query_with(&s, &alone, &legs, &fts(f), fusion, 20)
                    .await
                    .unwrap();
                same(&got, &want, &format!("{f} {q:?} {} legs", legs.len()));
                answered += usize::from(got.iter().any(|h| h.segment == 1));
            }
        }
        // `c` lives in segment 0 only: segment 1 answers nothing for it.
        if f == "c" {
            let got = query_with(
                &s,
                &many,
                &[text_leg("c", "revenue")],
                &fts("c"),
                sum1(),
                60,
            )
            .await
            .unwrap();
            assert!(!got.is_empty() && got.iter().all(|h| h.segment == 0));
        }
    }
    assert!(answered > 0, "no query reached the second segment");
}

fn sum1() -> Fusion {
    Fusion::Sum {
        weights: Weights::of(&[1.0]).unwrap(),
    }
}

#[tokio::test]
async fn what_one_field_per_query_cannot_answer_is_refused() {
    // M151 criterion 4, the query's half.
    let s = MemoryStore::new();
    let docs = rows(0, true);
    let three = put(&s, "m", &docs, &FIELDS).await;
    // An index of several fields is never opened as one.
    let seg = Segment::open(&s, &three.segment).await.unwrap();
    let raw = s.get(&text::dict_key(&three.segment)).await.unwrap();
    assert!(matches!(
        pstore_index::text::TextIndex::from_segment(&seg, &raw),
        Err(FormatError::UnknownField)
    ));
    assert!(pstore_index::text::TextIndex::for_field(&seg, &raw, "b").is_ok());
    let targets = [three.clone()];
    let two = [text_leg("a", "revenue"), text_leg("b", "revenue")];
    let e = query_with(&s, &targets, &two, &fts("a"), Fusion::Rrf { k: 60.0 }, 10)
        .await
        .unwrap_err();
    assert!(matches!(e, QueryError::Unimplemented(_)), "{e:?}");
    let legs: Vec<(usize, Prefetch)> = two.iter().cloned().enumerate().collect();
    let e = open_part(&s, &[(0, three.clone())], &legs, &[])
        .await
        .err()
        .expect("open_part took two fields");
    assert!(matches!(e, QueryError::Unimplemented(_)), "{e:?}");
    // A field no segment carries is a typo, never an empty answer.
    let e = query_with(&s, &targets, &[text_leg("d", "x")], &fts("a"), sum1(), 10)
        .await
        .unwrap_err();
    assert!(
        matches!(e, QueryError::UnknownTextField(ref f) if f == "d"),
        "{e:?}"
    );
}

#[tokio::test]
async fn a_share_without_the_field_adds_nothing_and_refuses_nothing() {
    // M151 criterion 4's last bullet: a phased share holds `c`, the coordinator's own segment
    // does not.
    let s = MemoryStore::new();
    let (one, two) = (rows(0, false), rows(60, true));
    let targets = [
        put(&s, "local", &one, &FIELDS).await,
        put(&s, "remote", &two, &FIELDS).await,
    ];
    let q = "revenue growth";
    let legs = [text_leg("c", q)];
    let indexed: Vec<(usize, Prefetch)> = legs.iter().cloned().enumerate().collect();
    let terms = text::analyze(&analyzer("c"), q);
    let (_, none) = open_part(&s, &[(0, targets[0].clone())], &indexed, &terms)
        .await
        .unwrap();
    assert_eq!(
        none.doc_count, 0,
        "a share without `c` added to its statistics"
    );

    let (part, stats) = open_part(&s, &[(1, targets[1].clone())], &indexed, &terms)
        .await
        .unwrap();
    assert!(stats.doc_count > 0);
    let store = s.clone();
    let ft = fts("c");
    let elsewhere = vec![Elsewhere {
        targets: vec![1],
        share: Share::Phased {
            stats: Box::pin(async move { Ok(stats) }),
            scan: Box::new(move |sum| {
                Box::pin(async move {
                    scan_part(&store, &part, None, 0, &sum, &ft, None)
                        .await
                        .map_err(|e| e.to_string())
                })
            }),
        },
    }];
    let got = query_rows_split(
        &s,
        &targets,
        &legs,
        &fts("c"),
        None,
        &std::collections::HashSet::new(),
        sum1(),
        20,
        elsewhere,
        false,
    )
    .await
    .unwrap();
    let alone = [
        put(&s, "c0", &one, &["c"]).await,
        put(&s, "c1", &two, &["c"]).await,
    ];
    let want = query_with(&s, &alone, &legs, &fts("c"), sum1(), 20)
        .await
        .unwrap();
    let got: Vec<Hit> = got.into_iter().map(|(h, _, _)| h).collect();
    assert!(!want.is_empty());
    same(&got, &want, "c through a phased share");
}

#[tokio::test]
async fn a_field_of_several_costs_what_one_field_costs() {
    // M151 criterion 5.
    let docs = rows(0, true);
    let t = TenantId(9);
    let acct = Accounted::new(MemoryStore::new());
    let counting = DepthCounting::new(acct.as_tenant(t));
    let three = put(&counting, "m", &docs, &FIELDS).await;
    let alone = put(&counting, "b", &docs, &["b"]).await;
    let legs = [text_leg("b", "running margin")];
    let mut costs = Vec::new();
    for target in [&three, &alone] {
        counting.reset();
        acct.record_ranges();
        let hits = query_with(
            &counting,
            std::slice::from_ref(target),
            &legs,
            &fts("b"),
            sum1(),
            10,
        )
        .await
        .unwrap();
        assert!(!hits.is_empty());
        let seg = Segment::open(counting.inner(), &target.segment)
            .await
            .unwrap();
        let (postings, norms) = (
            seg.section(Section::TextPostings).unwrap(),
            seg.section(Section::Fieldnorms).unwrap(),
        );
        let raw = counting
            .inner()
            .get(&text::dict_key(&target.segment))
            .await
            .unwrap();
        // The field's own slices: for one field the whole sections.
        let (p, n) = if seg.text_fields().len() == 1 {
            (postings, norms)
        } else {
            let d = text::Dictionaries::decode(&raw, 3).unwrap();
            let span = d.postings(1).unwrap();
            let width = (norms.end - norms.start) / 3;
            (
                postings.start + span.start..postings.start + span.end,
                norms.start + width..norms.start + 2 * width,
            )
        };
        costs.push((
            counting.depth(),
            counting.requests(),
            acct.bytes_in(&target.segment, p),
            acct.bytes_in(&target.segment, n),
            acct.bytes_in(&text::dict_key(&target.segment), 0..raw.len() as u64),
            raw.len() as u64,
        ));
    }
    let (many, one) = (costs[0], costs[1]);
    assert_eq!(
        (many.0, many.1),
        (one.0, one.1),
        "rounds and requests: {many:?} {one:?}"
    );
    assert_eq!(many.2, one.2, "the field's postings bytes");
    assert_eq!(many.3, one.3, "the field's norms bytes");
    assert!(one.3 > 0 && one.2 > 0);
    // The sidecar is fetched whole either way: the directory and the other dictionaries ride in it.
    assert_eq!(
        (many.4, one.4),
        (many.5, one.5),
        "the sidecar was not read whole"
    );
}

/// A segment over `docs` written by hand: `postings`, `norms` and `names` as given, so each
/// may disagree with the sidecar.
async fn forged(
    store: &MemoryStore,
    name: &str,
    docs: &[Document],
    postings: Vec<u8>,
    norms: Vec<u32>,
    names: &[&str],
) -> Segment {
    let mut w = pstore_format::SegmentWriter::new(8);
    for d in docs {
        w.push(d.clone());
    }
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let bytes = w
        .with_section(Section::TextPostings, postings)
        .with_section(Section::Fieldnorms, text::encode_norms(&norms))
        .with_text_fields(&names)
        .try_finish()
        .unwrap();
    let key = Key::new(format!("t/idx/{name}.seg"));
    store.put(&key, bytes).await.unwrap();
    Segment::open(store, &key).await.unwrap()
}

#[tokio::test]
async fn sections_that_disagree_with_the_sidecar_are_refused() {
    // M151 criterion 4: `for_field`'s three section checks (code review).
    use pstore_index::text::TextIndex;
    let s = MemoryStore::new();
    let docs = rows(0, true);
    let a = Analyzer::default();
    let built = text::build_fields(&docs, &[("a", &a), ("b", &a)]).unwrap();
    let raw = bytes::Bytes::from(built.dictionary.clone());
    let ok = forged(
        &s,
        "ok",
        &docs,
        built.postings.clone(),
        built.fieldnorms.clone(),
        &["a", "b"],
    )
    .await;
    for f in ["a", "b"] {
        assert!(TextIndex::for_field(&ok, &raw, f).is_ok(), "{f}");
    }
    let corrupt = |r: Result<TextIndex, FormatError>, what: &str| {
        assert!(matches!(r, Err(FormatError::Corrupt(_))), "{what}: {r:?}");
    };
    // The spans stop short of the postings section.
    let mut long = built.postings.clone();
    long.push(0);
    let seg = forged(
        &s,
        "long",
        &docs,
        long,
        built.fieldnorms.clone(),
        &["a", "b"],
    )
    .await;
    corrupt(
        TextIndex::for_field(&seg, &raw, "a"),
        "postings past the spans",
    );
    // One fieldnorm too many: not one per row per field.
    let mut norms = built.fieldnorms.clone();
    norms.push(0);
    let seg = forged(
        &s,
        "norms",
        &docs,
        built.postings.clone(),
        norms,
        &["a", "b"],
    )
    .await;
    corrupt(
        TextIndex::for_field(&seg, &raw, "b"),
        "a fieldnorm too many",
    );
    // A name twice.
    let seg = forged(
        &s,
        "twice",
        &docs,
        built.postings.clone(),
        built.fieldnorms,
        &["a", "a"],
    )
    .await;
    corrupt(TextIndex::for_field(&seg, &raw, "a"), "a name twice");
}

#[tokio::test]
async fn two_fields_are_refused_before_any_request_on_every_path() {
    // M151 criterion 4: a phased share's query and a one-call share refuse as `run` does,
    // before the open round (code review).
    let counting = DepthCounting::new(MemoryStore::new());
    let docs = rows(0, true);
    let targets = [
        put(&counting, "l", &docs, &FIELDS).await,
        put(&counting, "r", &docs, &FIELDS).await,
    ];
    let two = [text_leg("a", "revenue"), text_leg("b", "revenue")];
    counting.reset();
    let elsewhere = vec![Elsewhere {
        targets: vec![1],
        share: Share::Phased {
            stats: Box::pin(async { Ok(pstore_index::text::Stats::default()) }),
            scan: Box::new(|_| Box::pin(async { Ok(Vec::new()) })),
        },
    }];
    let e = query_rows_split(
        &counting,
        &targets,
        &two,
        &fts("a"),
        None,
        &std::collections::HashSet::new(),
        Fusion::Rrf { k: 60.0 },
        10,
        elsewhere,
        false,
    )
    .await
    .expect_err("a phased query took two fields");
    assert!(matches!(e, QueryError::Unimplemented(_)), "{e:?}");
    let legs: Vec<(usize, Prefetch)> = two.iter().cloned().enumerate().collect();
    let e = pstore_query::part(
        &counting,
        &[(0, targets[0].clone())],
        &legs,
        None,
        0,
        &pstore_index::text::Stats::default(),
        &fts("a"),
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, QueryError::Unimplemented(_)), "{e:?}");
    assert_eq!(counting.requests(), 0, "refused after a request");
}
