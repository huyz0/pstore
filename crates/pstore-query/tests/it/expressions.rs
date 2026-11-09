//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Exact scoring expressions (M182): `BM25`, `Sum`, `Max`, `Product` and a filter as a score,
//! against a reference that scores each row from its raw strings alone.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::text::{self, Analyzer, FullText, Language, TOKEN_SEPARATOR};
use pstore_format::{Document, Value};
use pstore_index::cluster::Params;
use pstore_index::vec_index;
use pstore_query::{
    DistOrigin as Origin, Expr, Fusion, Op, Predicate, Prefetch, Target, Texts, Weights,
    query_with_texts, rank,
};
use std::collections::{BTreeMap, HashSet};

const DIM: usize = 2;
/// English, stemmed, stopwords removed: the index's default.
const BODY: &str = "body";
/// A `[]string` field under the default analyzer, tokens at most 6 long.
const KW: &str = "kw";
const SEGMENTS: usize = 3;
const ROWS: usize = 30;

const BODY_WORDS: [&str; 14] = [
    "the", "and", "of", "run", "running", "runs", "fox", "foxes", "quick", "growth", "margin",
    "revenue", "is", "quickly",
];
const KW_WORDS: [&str; 9] = [
    "alpha",
    "beta",
    "international",
    "gamma",
    "abcdefghij",
    "delta",
    "longword",
    "short",
    "alphabet",
];
const TAGS: [&str; 3] = ["red", "green", "blue"];
/// M203: a signed int, a signed float and a datetime.
const N: &str = "n";
const F: &str = "f";
const AT: &str = "at";
/// 2026-01-01T00:00:00Z, in microseconds.
const T0: i64 = 1_767_225_600_000_000;

fn texts() -> Texts {
    Texts {
        default: FullText {
            analyzer: Analyzer {
                language: Language::English,
                stemming: true,
                remove_stopwords: true,
                ..Analyzer::default()
            },
            ..FullText::default()
        },
        fields: BTreeMap::from([(
            KW.to_owned(),
            FullText {
                analyzer: Analyzer {
                    max_token_length: Some(6),
                    ..Analyzer::default()
                },
                k1: 1.6,
                b: 0.4,
            },
        )]),
    }
}

/// A seeded generator: the same trees on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[self.below(of.len())]
    }
}

/// Row `i`: a body of 2 to 7 words but every seventh lacking one, keywords but every fifth
/// lacking them, and a tag. Ids are not in row order, so an order by row is not one by id.
fn row(i: usize, rng: &mut Rng) -> Document {
    let mut d = Document::new(format!("d{:03}", (i * 37) % 90), vec![0.5; DIM]);
    if !i.is_multiple_of(7) {
        let n = 2 + rng.below(6);
        let words: Vec<&str> = (0..n).map(|_| rng.pick(&BODY_WORDS)).collect();
        d.attrs.insert(BODY.into(), Value::Str(words.join(" ")));
    }
    if !i.is_multiple_of(5) {
        let n = 1 + rng.below(4);
        d.attrs.insert(
            KW.into(),
            Value::Array(
                (0..n)
                    .map(|_| Value::Str(rng.pick(&KW_WORDS).to_owned()))
                    .collect(),
            ),
        );
    }
    d.attrs
        .insert("tag".into(), Value::Str(TAGS[i % 3].to_owned()));
    numeric_attrs(i, &mut d);
    d
}

/// M203's attributes, from `i` alone so M182's draws are unchanged: a signed int `n`, a
/// string on a few rows; a signed float `f`; a datetime `at`, an hour apart. Each is missing
/// on some rows.
fn numeric_attrs(i: usize, d: &mut Document) {
    if i % 11 == 3 {
        d.attrs.insert(N.into(), Value::Str("seven".into()));
    } else if i % 4 != 1 {
        d.attrs
            .insert(N.into(), Value::Int(((i * 7919) % 21) as i64 - 10));
    }
    if i % 6 != 2 {
        d.attrs
            .insert(F.into(), Value::Float(((i * 31) % 17) as f64 * 0.37 - 2.5));
    }
    if i % 9 != 4 {
        d.attrs
            .insert(AT.into(), Value::DateTime(T0 + i as i64 * 3_600_000_000));
    }
}

/// The corpus: each segment's documents, and the rows of each its delete vector names.
struct Corpus {
    store: MemoryStore,
    targets: Vec<Target>,
    segments: Vec<Vec<Document>>,
    deleted: Vec<HashSet<usize>>,
}

async fn put<S: BlobStore>(
    store: &S,
    name: &str,
    docs: &[Document],
    deleted: &HashSet<usize>,
) -> Target {
    let t = texts();
    let fields: Vec<(&str, &Analyzer)> = [BODY, KW]
        .into_iter()
        .map(|f| (f, &t.of(f).analyzer))
        .filter(|(f, _)| {
            docs.iter()
                .any(|d| d.attrs.get(*f).is_some_and(text::has_text))
        })
        .collect();
    let built = vec_index::try_build_all_fields(
        docs,
        Params {
            exact_scan_threshold: 10_000,
            ..Params::default()
        },
        pstore_format::DEFAULT_FIELD,
        None,
        &fields,
        &[],
    )
    .unwrap();
    let key = Key::new(format!("t/idx/{name}.seg"));
    store.put(&key, built.segment).await.unwrap();
    let text_dict = built.text_dictionary.is_some();
    if let Some(d) = built.text_dictionary {
        store
            .put(&text::dict_key(&key), bytes::Bytes::from(d))
            .await
            .unwrap();
    }
    let dv = Key::new(format!("t/idx/{name}.del"));
    store
        .put(
            &dv,
            bytes::Bytes::from(pstore_query::deletes::encode(deleted)),
        )
        .await
        .unwrap();
    Target {
        segment_len: None,
        centroids: None,
        deleted: Some(dv),
        sparse_dict: false,
        text_dict,
        fde_clusters: false,
        shadowed: false,
        segment: key,
    }
}

/// The corpus; with `fresh`, the last segment has no deleted rows, so it can stand for the
/// unfolded rows: the same rows, sealed or not.
async fn corpus(fresh: bool) -> Corpus {
    let store = MemoryStore::new();
    let mut rng = Rng(0x5eed_1820);
    let mut out = Corpus {
        store,
        targets: Vec::new(),
        segments: Vec::new(),
        deleted: Vec::new(),
    };
    for s in 0..SEGMENTS {
        let docs: Vec<Document> = (0..ROWS).map(|r| row(s * ROWS + r, &mut rng)).collect();
        let deleted: HashSet<usize> = if fresh && s == SEGMENTS - 1 {
            HashSet::new()
        } else {
            [3, 11, 20 + s].into()
        };
        out.targets
            .push(put(&out.store, &format!("s{s}"), &docs, &deleted).await);
        out.segments.push(docs);
        out.deleted.push(deleted);
    }
    out
}

impl Corpus {
    /// Every row a query sees: not deleted.
    fn live(&self) -> impl Iterator<Item = &Document> {
        self.segments
            .iter()
            .zip(&self.deleted)
            .flat_map(|(docs, del)| {
                docs.iter()
                    .enumerate()
                    .filter(|(r, _)| !del.contains(r))
                    .map(|(_, d)| d)
            })
    }
}

// --- The reference: raw strings, `analyze`, BM25 in f64 ---------------------------------

fn strings(v: &Value) -> Vec<&str> {
    match v {
        Value::Str(s) => vec![s.as_str()],
        Value::Array(a) => a
            .iter()
            .filter_map(|e| match e {
                Value::Str(s) => Some(s.as_str()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn terms_of(d: &Document, field: &str) -> Vec<String> {
    let a = texts().of(field).analyzer;
    d.attrs
        .get(field)
        .map(|v| {
            strings(v)
                .into_iter()
                .flat_map(|s| text::analyze(&a, s))
                .collect()
        })
        .unwrap_or_default()
}

/// A field's statistics: every sealed row of every segment where some row has text under it.
struct RefStats {
    n: f64,
    avgdl: f64,
    df: BTreeMap<String, f64>,
}

fn ref_stats(c: &Corpus, field: &str) -> RefStats {
    let mut n = 0.0;
    let mut total = 0.0;
    let mut df: BTreeMap<String, f64> = BTreeMap::new();
    for docs in &c.segments {
        if !docs
            .iter()
            .any(|d| d.attrs.get(field).is_some_and(text::has_text))
        {
            continue;
        }
        for d in docs {
            let t = terms_of(d, field);
            n += 1.0;
            total += t.len() as f64;
            let distinct: std::collections::BTreeSet<&String> = t.iter().collect();
            for term in distinct {
                *df.entry(term.clone()).or_insert(0.0) += 1.0;
            }
        }
    }
    RefStats {
        n,
        avgdl: if n > 0.0 { total / n } else { 0.0 },
        df,
    }
}

/// A BM25 clause's terms and prefix forms, from the raw query alone.
fn ref_query(field: &str, q: &str, last_as_prefix: bool) -> (Vec<String>, Vec<String>) {
    let a = texts().of(field).analyzer;
    let toks = text::query_tokens(&a, q);
    match (last_as_prefix, toks.split_last()) {
        (true, Some((last, before))) => {
            let raw = &q[last.span.clone()];
            let plain = Analyzer {
                stemming: false,
                remove_stopwords: false,
                ..a
            };
            let mut forms = text::analyze(&plain, raw);
            forms.extend(last.term.clone());
            (
                before.iter().filter_map(|t| t.term.clone()).collect(),
                forms,
            )
        }
        _ => (text::analyze_query(&a, q), Vec::new()),
    }
}

fn ref_bm25(
    stats: &BTreeMap<&str, RefStats>,
    d: &Document,
    field: &str,
    q: &str,
    last_as_prefix: bool,
) -> f64 {
    let ft = *texts().of(field);
    let (k1, b) = (f64::from(ft.k1), f64::from(ft.b));
    let st = &stats[field];
    let row = terms_of(d, field);
    let (mut terms, forms) = ref_query(field, q, last_as_prefix);
    terms.sort();
    terms.dedup();
    let len = row.len() as f64;
    let mut score = 0.0;
    for t in &terms {
        let tf = row.iter().filter(|r| *r == t).count() as f64;
        if tf == 0.0 {
            continue;
        }
        let df = st.df[t];
        let idf = (1.0 + (st.n - df + 0.5) / (df + 0.5)).ln();
        let norm = if st.avgdl > 0.0 {
            k1 * (1.0 - b + b * len / st.avgdl)
        } else {
            k1 * (1.0 - b)
        };
        score += idf * tf * (k1 + 1.0) / (tf + norm);
    }
    if row
        .iter()
        .any(|r| forms.iter().any(|f| r.starts_with(f.as_str())))
    {
        score += 1.0;
    }
    score
}

fn ref_score(stats: &BTreeMap<&str, RefStats>, e: &Expr, d: &Document) -> f64 {
    match e {
        Expr::Bm25 {
            field,
            query,
            last_as_prefix,
        } => ref_bm25(stats, d, field, query, *last_as_prefix),
        Expr::Sum(all) => all.iter().map(|e| ref_score(stats, e, d)).sum(),
        Expr::Max(all) => all
            .iter()
            .map(|e| ref_score(stats, e, d))
            .fold(f64::NEG_INFINITY, f64::max),
        Expr::Product(w, e) => f64::from(*w) * ref_score(stats, e, d),
        Expr::Filter(p) => f64::from(u8::from(p.admits(&d.id, &d.attrs))),
        _ => ref_value(stats, e, d).unwrap_or(0.0),
    }
}

/// M203's nodes, as the vendor writes them: `None` is no value.
fn ref_value(stats: &BTreeMap<&str, RefStats>, e: &Expr, d: &Document) -> Option<f64> {
    let number = |a: &str| match d.attrs.get(a)? {
        Value::Int(v) => Some(*v as f64),
        Value::Uint(v) => Some(*v as f64),
        Value::Float(v) => Some(v.clamp(-1e30, 1e30)),
        _ => None,
    };
    match e {
        Expr::Attribute(a) => number(a),
        Expr::Literal(v) => Some(*v),
        // No value scores 0 under both; a value is clamped at 0, then the formula as written.
        Expr::Saturate {
            of,
            midpoint,
            exponent,
        } => Some(ref_value(stats, of, d).map_or(0.0, |x| {
            let x = x.max(0.0).powf(*exponent);
            x / (x + midpoint.powf(*exponent))
        })),
        Expr::Decay {
            of,
            midpoint,
            exponent,
        } => Some(ref_value(stats, of, d).map_or(0.0, |x| {
            let m = midpoint.powf(*exponent);
            m / (x.max(0.0).powf(*exponent) + m)
        })),
        Expr::Dist { attr, origin } => match (origin, d.attrs.get(attr)?) {
            (Origin::Number(o), _) => number(attr).map(|v| (v - o).abs()),
            (Origin::Instant(o), Value::DateTime(t)) => Some(((t - o) as f64 / 1000.0).abs()),
            (Origin::Instant(_), _) => None,
        },
        _ => Some(ref_score(stats, e, d)),
    }
}

/// The reference's answer: every live row `filter` admits that scores above 0, by score
/// descending then id.
fn reference(c: &Corpus, e: &Expr, filter: Option<&Predicate>) -> Vec<(String, f64)> {
    let stats: BTreeMap<&str, RefStats> = [BODY, KW]
        .into_iter()
        .map(|f| (f, ref_stats(c, f)))
        .collect();
    let mut out: Vec<(String, f64)> = c
        .live()
        .filter(|d| filter.is_none_or(|f| f.admits(&d.id, &d.attrs)))
        .map(|d| (d.id.clone(), ref_score(&stats, e, d)))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

// --- The generator -------------------------------------------------------------------------

fn tag(rng: &mut Rng) -> Predicate {
    Predicate::Cmp("tag".into(), Op::Eq, Value::Str(rng.pick(&TAGS).to_owned()))
}

fn bm25(rng: &mut Rng) -> Expr {
    let field = if rng.below(2) == 0 { BODY } else { KW };
    let words: &[&str] = if field == BODY {
        &BODY_WORDS
    } else {
        &KW_WORDS
    };
    let n = 1 + rng.below(3);
    let parts: Vec<&str> = (0..n)
        .map(|_| {
            let w = rng.pick(words);
            // A prefix of a word now and then, for `last_as_prefix` to have something to do.
            if rng.below(3) == 0 {
                &w[..w.len().min(1 + rng.below(4))]
            } else {
                w
            }
        })
        .collect();
    // A token array is the parts joined by the separator, as the server makes one (M158).
    let sep = if rng.below(4) == 0 {
        TOKEN_SEPARATOR.to_string()
    } else {
        " ".to_owned()
    };
    Expr::Bm25 {
        field: field.to_owned(),
        query: parts.join(&sep),
        last_as_prefix: rng.below(3) == 0,
    }
}

const WEIGHTS: [f32; 6] = [0.0, 1e-3, 0.5, 1.0, 2.5, 1e3];

fn tree(rng: &mut Rng, depth: usize) -> Expr {
    let leaf = depth >= 5 || rng.below(3) == 0;
    if leaf {
        return match rng.below(4) {
            0 => Expr::Filter(match rng.below(3) {
                0 => tag(rng),
                1 => Predicate::Not(Box::new(tag(rng))),
                _ => Predicate::Absent(BODY.into()),
            }),
            _ => bm25(rng),
        };
    }
    match rng.below(3) {
        0 => Expr::Sum(
            (0..1 + rng.below(4))
                .map(|_| tree(rng, depth + 1))
                .collect(),
        ),
        1 => Expr::Max(
            (0..1 + rng.below(4))
                .map(|_| tree(rng, depth + 1))
                .collect(),
        ),
        _ => Expr::Product(
            WEIGHTS[rng.below(WEIGHTS.len())],
            Box::new(tree(rng, depth + 1)),
        ),
    }
}

/// `got` against the reference: the same rows exactly, each score within `tol` relative, in
/// the reference's order wherever its adjacent scores differ by more than `tol`, and by id
/// within a tie both answers agree on.
fn assert_matches(got: &[(String, f64)], want: &[(String, f64)], tol: f64, what: &str) {
    let mut got_ids: Vec<&String> = got.iter().map(|(id, _)| id).collect();
    let mut want_ids: Vec<&String> = want.iter().map(|(id, _)| id).collect();
    got_ids.sort();
    want_ids.sort();
    assert_eq!(got_ids, want_ids, "kept rows: {what}");
    let by_id: BTreeMap<&String, f64> = want.iter().map(|(id, s)| (id, *s)).collect();
    for (id, s) in got {
        let w = by_id[id];
        assert!((*s - w).abs() <= tol * w, "{id}: {s} against {w}: {what}");
    }
    for pair in got.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let (wa, wb) = (by_id[&a.0], by_id[&b.0]);
        if wb > wa * (1.0 + tol) {
            panic!("{} ({wa}) before {} ({wb}): {what}", a.0, b.0);
        }
        // A tie is one in the answer: the reference's equal scores can be a bit apart here,
        // its leaves being f64 and these f32 widened.
        if wa == wb && a.1 == b.1 {
            assert!(
                a.0 < b.0,
                "a tie out of id order, {} then {}: {what}",
                a.0,
                b.0
            );
        }
    }
}

async fn ranked(c: &Corpus, e: &Expr, filter: Option<&Predicate>) -> Vec<(String, f64)> {
    ranked_in(c, e, filter, false).await
}

/// With `fresh`, the last segment's rows are passed as the unfolded rows rather than sealed.
async fn ranked_in(
    c: &Corpus,
    e: &Expr,
    filter: Option<&Predicate>,
    fresh: bool,
) -> Vec<(String, f64)> {
    let (targets, unfolded): (&[Target], &[Document]) = if fresh {
        (&c.targets[..SEGMENTS - 1], &c.segments[SEGMENTS - 1])
    } else {
        (&c.targets, &[])
    };
    rank(
        &c.store,
        targets,
        filter,
        &HashSet::new(),
        e,
        &texts(),
        unfolded,
        1_000,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|(d, s)| (d.id, s))
    .collect()
}

/// Criterion 1: 300 seeded trees, every node kind, up to depth 5.
#[tokio::test]
async fn expressions_match_the_oracle() {
    let c = corpus(false).await;
    let mut rng = Rng(0x0182_0182);
    let mut nonempty = 0;
    for i in 0..300 {
        // A tree over the node limit is drawn again: the limits are the refusal test's.
        let e = std::iter::repeat_with(|| tree(&mut rng, 1))
            .find(|e| e.check().is_ok())
            .unwrap();
        let filter = (i % 3 == 0).then(|| tag(&mut rng));
        let want = reference(&c, &e, filter.as_ref());
        let got = ranked(&c, &e, filter.as_ref()).await;
        assert_matches(&got, &want, 1e-4, &format!("tree {i}: {e:?}"));
        nonempty += usize::from(!want.is_empty());
    }
    // The generator is not vacuous: most trees keep some row.
    assert!(nonempty > 150, "{nonempty} of 300 trees kept a row");
}

fn leg(field: &str, q: &str, last_as_prefix: bool) -> Prefetch {
    Prefetch::Text {
        field: field.to_owned(),
        query: q.to_owned(),
        limit: 500,
        last_as_prefix,
    }
}

/// Criterion 2: a lone BM25 clause returns the text leg's hits, at the leg's scores -- and
/// so it does with a segment's rows unfolded, which the leg reads sealed.
#[tokio::test]
async fn a_bm25_clause_is_the_text_leg() {
    for fresh in [false, true] {
        a_bm25_clause_is_the_leg_in(fresh).await;
    }
}

async fn a_bm25_clause_is_the_leg_in(fresh: bool) {
    let c = corpus(fresh).await;
    let mut rng = Rng(0x1e9_1e9);
    let mut compared = 0;
    for field in [BODY, KW] {
        let words: &[&str] = if field == BODY {
            &BODY_WORDS
        } else {
            &KW_WORDS
        };
        for _ in 0..20 {
            let n = 1 + rng.below(3);
            let q: Vec<&str> = (0..n)
                .map(|_| {
                    let w = rng.pick(words);
                    &w[..w.len().min(2 + rng.below(6))]
                })
                .collect();
            let q = q.join(" ");
            for last_as_prefix in [false, true] {
                let hits = query_with_texts(
                    &c.store,
                    &c.targets,
                    &[leg(field, &q, last_as_prefix)],
                    &texts(),
                    Fusion::Sum {
                        weights: Weights::of(&[1.0]).unwrap(),
                    },
                    500,
                )
                .await
                .unwrap();
                // The leg's scores, keyed by id, in its order.
                let want: Vec<(String, f64)> = hits
                    .iter()
                    .map(|h| (c.segments[h.segment][h.row].id.clone(), f64::from(h.score)))
                    .collect();
                let e = Expr::Bm25 {
                    field: field.to_owned(),
                    query: q.clone(),
                    last_as_prefix,
                };
                let got = ranked_in(&c, &e, None, fresh).await;
                assert_matches(
                    &got,
                    &want,
                    1e-5,
                    &format!("{field} {q:?} prefix {last_as_prefix} fresh {fresh}"),
                );
                compared += want.len();
            }
        }
    }
    assert!(compared > 200, "{compared} hits compared");
}

/// Criterion 3: a row scoring exactly 0 is absent; a filter-only expression is the filter.
#[tokio::test]
async fn zero_scores_are_dropped() {
    let c = corpus(false).await;
    let red = Predicate::Cmp("tag".into(), Op::Eq, Value::Str("red".into()));
    let never = Predicate::And(vec![red.clone(), Predicate::Not(Box::new(red.clone()))]);
    let e = Expr::Sum(vec![
        Expr::Bm25 {
            field: BODY.into(),
            query: "fox".into(),
            last_as_prefix: false,
        },
        Expr::Product(
            0.0,
            Box::new(Expr::Bm25 {
                field: BODY.into(),
                query: "growth".into(),
                last_as_prefix: false,
            }),
        ),
        Expr::Filter(never),
    ]);
    let got = ranked(&c, &e, None).await;
    let fox: HashSet<String> = c
        .live()
        .filter(|d| terms_of(d, BODY).contains(&"fox".to_owned()))
        .map(|d| d.id.clone())
        .collect();
    assert!(!fox.is_empty());
    let kept: HashSet<String> = got.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(kept, fox, "only the rows the one positive clause hits");
    assert!(got.iter().all(|(_, s)| *s > 0.0));

    let e = Expr::Product(2.5, Box::new(Expr::Filter(red.clone())));
    let got = ranked(&c, &e, None).await;
    let mut want: Vec<String> = c
        .live()
        .filter(|d| red.admits(&d.id, &d.attrs))
        .map(|d| d.id.clone())
        .collect();
    want.sort();
    let ids: Vec<String> = got.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(ids, want, "exactly the filter's rows, by id");
    assert!(got.iter().all(|(_, s)| *s == 2.5), "{got:?}");
}

// --- M203: Attribute, literals, Saturate, Decay, Dist -------------------------------------

const MIDPOINTS: [f64; 4] = [1e-3, 0.5, 3.0, 7_200_000.0];
const EXPONENTS: [f64; 4] = [0.5, 1.0, 2.0, 3.5];
const LITERALS: [f64; 5] = [-4.0, -0.5, 0.0, 1.0, 2.5];

/// A leaf or node of M203's kinds, or M182's.
fn numeric_tree(rng: &mut Rng, depth: usize) -> Expr {
    let attribute = |rng: &mut Rng| Expr::Attribute([N, F, AT][rng.below(3)].to_owned());
    let dist = |rng: &mut Rng| match rng.below(3) {
        0 => Expr::Dist {
            attr: AT.into(),
            origin: Origin::Instant(T0 + 40 * 3_600_000_000),
        },
        1 => Expr::Dist {
            attr: N.into(),
            origin: Origin::Number(2.0),
        },
        _ => Expr::Dist {
            attr: F.into(),
            origin: Origin::Number(-1.0),
        },
    };
    if depth >= 5 || rng.below(3) == 0 {
        return match rng.below(6) {
            0 => attribute(rng),
            1 => Expr::Literal(LITERALS[rng.below(LITERALS.len())]),
            2 => dist(rng),
            3 => Expr::Filter(tag(rng)),
            _ => bm25(rng),
        };
    }
    let child = |rng: &mut Rng| Box::new(numeric_tree(rng, depth + 1));
    // A Saturate's or Decay's argument stays within 1e-3..1e6, where the vendor's formula as
    // written neither under- nor overflows.
    let argument = |rng: &mut Rng| {
        Box::new(match rng.below(4) {
            0 => attribute(rng),
            1 => dist(rng),
            2 => Expr::Literal(LITERALS[rng.below(LITERALS.len())]),
            _ => Expr::Sum(vec![
                attribute(rng),
                Expr::Literal(LITERALS[rng.below(LITERALS.len())]),
            ]),
        })
    };
    let midpoint = MIDPOINTS[rng.below(MIDPOINTS.len())];
    let exponent = EXPONENTS[rng.below(EXPONENTS.len())];
    match rng.below(6) {
        0 => Expr::Sum((0..1 + rng.below(4)).map(|_| *child(rng)).collect()),
        1 => Expr::Max((0..1 + rng.below(4)).map(|_| *child(rng)).collect()),
        2 => Expr::Product(WEIGHTS[rng.below(WEIGHTS.len())], child(rng)),
        3 => Expr::Saturate {
            of: argument(rng),
            midpoint,
            exponent,
        },
        4 => Expr::Decay {
            of: argument(rng),
            midpoint,
            exponent,
        },
        _ => Expr::Max(vec![Expr::Literal(0.0), *child(rng)]),
    }
}

/// M203 criterion 1: 300 seeded trees over every node kind, up to depth 5, each root
/// non-negative: a signed one is drawn again.
#[tokio::test]
async fn numeric_expressions_match_the_oracle() {
    let c = corpus(false).await;
    let mut rng = Rng(0x0203_0203);
    let mut nonempty = 0;
    let mut kinds = BTreeMap::<&str, usize>::new();
    for i in 0..300 {
        let e = std::iter::repeat_with(|| numeric_tree(&mut rng, 1))
            .find(|e| e.check().is_ok())
            .unwrap();
        let s = format!("{e:?}");
        for k in [
            "Attribute",
            "Literal(-",
            "Saturate",
            "Decay",
            "Dist",
            "Bm25",
            "Max",
        ] {
            *kinds.entry(k).or_insert(0) += usize::from(s.contains(k));
        }
        let filter = (i % 3 == 0).then(|| tag(&mut rng));
        let want = reference(&c, &e, filter.as_ref());
        let got = ranked(&c, &e, filter.as_ref()).await;
        assert_matches(&got, &want, 1e-4, &format!("tree {i}: {e:?}"));
        nonempty += usize::from(!want.is_empty());
    }
    assert!(nonempty > 150, "{nonempty} of 300 trees kept a row");
    // Every kind is drawn often, a negative literal among them.
    assert!(kinds.values().all(|n| *n >= 30), "{kinds:?}");
}

/// M203 criterion 2: the root is non-negative by its form, or refused.
#[test]
fn signed_roots_are_refused() {
    let a = || Expr::Attribute(N.into());
    let bm = || Expr::Bm25 {
        field: BODY.into(),
        query: "fox".into(),
        last_as_prefix: false,
    };
    let sat = || Expr::Saturate {
        of: Box::new(a()),
        midpoint: 1.0,
        exponent: 1.0,
    };
    for signed in [
        a(),
        Expr::Literal(-1.0),
        Expr::Sum(vec![bm(), a()]),
        Expr::Product(2.0, Box::new(a())),
        Expr::Max(vec![a(), Expr::Literal(-1.0)]),
        Expr::Sum(vec![Expr::Max(vec![a()]), bm()]),
    ] {
        let why = signed.check().unwrap_err();
        assert!(why.contains("negative"), "{signed:?}: {why}");
    }
    for ok in [
        Expr::Max(vec![Expr::Literal(0.0), a()]),
        Expr::Max(vec![a(), bm()]),
        sat(),
        Expr::Sum(vec![sat(), Expr::Literal(1.0), bm()]),
        Expr::Product(
            2.0,
            Box::new(Expr::Max(vec![
                Expr::Sum(vec![a(), Expr::Literal(-3.0)]),
                Expr::Literal(0.0),
            ])),
        ),
        Expr::Decay {
            of: Box::new(Expr::Sum(vec![a(), Expr::Literal(-3.0)])),
            midpoint: 1.0,
            exponent: 1.0,
        },
        Expr::Dist {
            attr: N.into(),
            origin: Origin::Number(-5.0),
        },
    ] {
        assert!(ok.check().is_ok(), "{ok:?}: {:?}", ok.check());
    }
    for bad in [
        Expr::Literal(f64::NAN),
        Expr::Literal(f64::INFINITY),
        Expr::Literal(1.1e30),
        Expr::Saturate {
            of: Box::new(a()),
            midpoint: 0.0,
            exponent: 1.0,
        },
        Expr::Saturate {
            of: Box::new(a()),
            midpoint: 1.1e30,
            exponent: 1.0,
        },
        Expr::Decay {
            of: Box::new(a()),
            midpoint: 1.0,
            exponent: 0.0,
        },
        Expr::Decay {
            of: Box::new(a()),
            midpoint: 1.0,
            exponent: 16.5,
        },
        Expr::Dist {
            attr: N.into(),
            origin: Origin::Number(f64::NAN),
        },
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
    let at_limits = Expr::Decay {
        of: Box::new(a()),
        midpoint: 1e-9,
        exponent: 16.0,
    };
    assert!(at_limits.check().is_ok());
}

/// M203 criterion 3: no value scores 0, and a value below 0 is clamped to 0.
#[tokio::test]
async fn missing_values_score_zero() {
    let c = corpus(false).await;
    let ids = |got: &[(String, f64)]| -> Vec<String> {
        let mut v: Vec<String> = got.iter().map(|(id, _)| id.clone()).collect();
        v.sort();
        v
    };
    let n_of = |d: &Document| match d.attrs.get(N) {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    };
    let positive = Expr::Max(vec![Expr::Literal(0.0), Expr::Attribute(N.into())]);
    let got = ranked(&c, &positive, None).await;
    let mut want: Vec<String> = c
        .live()
        .filter(|d| n_of(d).is_some_and(|v| v > 0))
        .map(|d| d.id.clone())
        .collect();
    want.sort();
    assert!(!want.is_empty());
    assert_eq!(ids(&got), want, "exactly the rows whose n > 0");

    let decay = Expr::Decay {
        of: Box::new(Expr::Attribute(N.into())),
        midpoint: 3.0,
        exponent: 1.0,
    };
    let got = ranked(&c, &decay, None).await;
    let mut want: Vec<String> = c
        .live()
        .filter(|d| n_of(d).is_some())
        .map(|d| d.id.clone())
        .collect();
    want.sort();
    assert_eq!(
        ids(&got),
        want,
        "no row lacking n, nor one holding a string"
    );
    let by_id: BTreeMap<String, f64> = got.into_iter().collect();
    let mut clamped = 0;
    for d in c.live() {
        if let Some(v) = n_of(d)
            && v <= 0
        {
            assert_eq!(by_id[&d.id], 1.0, "{}: n = {v} decays from 0", d.id);
            clamped += 1;
        }
    }
    assert!(clamped > 5, "{clamped} rows at or below 0");

    // A Max of signed clauses alone is their largest, not 0: inside a sum it moves the sum.
    let shifted = Expr::Max(vec![
        Expr::Literal(0.0),
        Expr::Sum(vec![
            Expr::Max(vec![Expr::Attribute(N.into())]),
            Expr::Literal(5.0),
        ]),
    ]);
    let got: BTreeMap<String, f64> = ranked(&c, &shifted, None).await.into_iter().collect();
    let mut negative = 0;
    for d in c.live() {
        match n_of(d) {
            Some(v) if v < 5 && v > -5 => {
                assert_eq!(got[&d.id], (v + 5) as f64, "{}: n = {v}", d.id);
                negative += usize::from(v < 0);
            }
            Some(v) if v <= -5 => assert!(!got.contains_key(&d.id), "{}: n = {v}", d.id),
            _ => {}
        }
    }
    assert!(negative > 3, "{negative} rows with -5 < n < 0");

    // A number origin gives a datetime no distance, so no row is kept.
    let dist = Expr::Saturate {
        of: Box::new(Expr::Dist {
            attr: AT.into(),
            origin: Origin::Number(0.0),
        }),
        midpoint: 1.0,
        exponent: 1.0,
    };
    assert!(ranked(&c, &dist, None).await.is_empty());
    // And an instant origin gives it one, in milliseconds: an hour from row 40's.
    let dist = Expr::Dist {
        attr: AT.into(),
        origin: Origin::Instant(T0 + 40 * 3_600_000_000),
    };
    let got: BTreeMap<String, f64> = ranked(&c, &dist, None).await.into_iter().collect();
    let row41 = &c.segments[1][11];
    assert!(
        c.deleted[1].contains(&11),
        "row 41 is deleted in this corpus"
    );
    let row42 = &c.segments[1][12];
    assert_eq!(got[&row42.id], 7_200_000.0, "two hours, in milliseconds");
    assert!(!got.contains_key(&row41.id));

    // A float beyond 1e30 reads as 1e30, so two of them sum to a finite 2e30.
    let mut huge = Document::new("huge", vec![0.5; DIM]);
    huge.attrs.insert(F.into(), Value::Float(1e300));
    let twice = Expr::Max(vec![
        Expr::Literal(0.0),
        Expr::Sum(vec![Expr::Attribute(F.into()), Expr::Attribute(F.into())]),
    ]);
    let got = rank(
        &c.store,
        &[],
        None,
        &HashSet::new(),
        &twice,
        &texts(),
        &[huge],
        10,
    )
    .await
    .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, 2e30, "clamped to 1e30 each");
}

/// The limits [`Expr::check`] holds.
#[test]
fn expressions_out_of_bounds_are_refused() {
    let leaf = || Expr::Filter(Predicate::Absent("x".into()));
    assert!(Expr::Sum(vec![]).check().is_err());
    assert!(Expr::Max(vec![leaf(); 17]).check().is_err());
    assert!(Expr::Max(vec![leaf(); 16]).check().is_ok());
    for w in [-1.0, f32::NAN, f32::INFINITY, 9e-4, 1001.0] {
        assert!(Expr::Product(w, Box::new(leaf())).check().is_err(), "{w}");
    }
    for w in [0.0, 1e-3, 1e3] {
        assert!(Expr::Product(w, Box::new(leaf())).check().is_ok(), "{w}");
    }
    let mut deep = leaf();
    for _ in 0..7 {
        deep = Expr::Product(1.0, Box::new(deep));
    }
    assert!(deep.check().is_ok(), "depth 8");
    assert!(
        Expr::Product(1.0, Box::new(deep)).check().is_err(),
        "depth 9"
    );
    // Sum and Max nest as Product does: one clause each, 9 deep.
    let mut deep = leaf();
    for _ in 0..7 {
        deep = Expr::Sum(vec![deep]);
    }
    assert!(
        Expr::Max(vec![deep.clone()]).check().is_err(),
        "Max at depth 9"
    );
    assert!(deep.check().is_ok(), "Sum at depth 8");
    assert!(Expr::Sum(vec![deep]).check().is_err(), "Sum at depth 9");
    // 1 + 16 * (1 + 3) = 65 nodes, at depth 3.
    let wide = Expr::Sum(vec![Expr::Sum(vec![leaf(); 3]); 16]);
    let why = wide.check().unwrap_err();
    assert!(why.contains("64 nodes"), "65 nodes: {why}");
    // 1 + 15 * 4 + 3 = 64 nodes: the limit itself is accepted.
    let mut at_limit = vec![Expr::Sum(vec![leaf(); 3]); 15];
    at_limit.push(Expr::Sum(vec![leaf(); 2]));
    assert!(Expr::Sum(at_limit).check().is_ok(), "64 nodes");
    let wide = Expr::Sum(vec![Expr::Sum(vec![leaf(); 2]); 16]);
    assert!(wide.check().is_ok(), "49 nodes");
}
