//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M152 -- an index's further text fields, in the engine: declared, recorded, sealed, merged
//! and queried, each scoring exactly as an index whose only text field it is.

use pstore_blob::MemoryStore;
use pstore_engine::{Declared, Engine, EngineError, Head, Metric, Patch};
use pstore_format::text::{Analyzer, FullText, Language};
use pstore_format::{Document, Value};
use pstore_query::{Fusion, Prefetch, Weights};
use pstore_types::{Epoch, LaneId, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;

fn title_ft() -> FullText {
    FullText {
        analyzer: Analyzer {
            language: Language::English,
            stemming: true,
            remove_stopwords: true,
            ..Analyzer::default()
        },
        k1: 2.0,
        b: 0.3,
    }
}

fn tags_ft() -> FullText {
    FullText {
        analyzer: Analyzer {
            case_sensitive: true,
            ..Analyzer::default()
        },
        k1: 0.8,
        b: 1.0,
    }
}

fn further() -> BTreeMap<String, FullText> {
    BTreeMap::from([
        ("title".to_owned(), title_ft()),
        ("tags".to_owned(), tags_ft()),
    ])
}

fn declaring(texts: BTreeMap<String, FullText>) -> Declared {
    Declared {
        texts: Some(texts),
        ..Declared::default()
    }
}

fn strs(v: &[&str]) -> Value {
    Value::Array(v.iter().map(|s| Value::Str((*s).to_owned())).collect())
}

/// Row `i`: each field a string, an array, absent, or (for `title`) stopwords alone.
fn row(i: u32) -> Document {
    let x = i as f32;
    let mut d = Document::new(format!("d{i:04}"), vec![x.sin(), x.cos(), 1.0]);
    let a = &mut d.attrs;
    match i % 4 {
        0 => {
            a.insert("text".into(), Value::Str(format!("alpha beta n{}", i % 7)));
        }
        1 => {
            a.insert("text".into(), strs(&["gamma alpha", "delta"]));
        }
        2 => {}
        _ => {
            a.insert(
                "text".into(),
                Value::Str(format!("beta gamma gamma n{}", i % 5)),
            );
        }
    }
    match i % 5 {
        0 => {
            a.insert("title".into(), Value::Str("The running reports".into()));
        }
        1 => {
            a.insert(
                "title".into(),
                strs(&["quarterly report", "runs of the year"]),
            );
        }
        2 => {
            a.insert("title".into(), Value::Str("the of and".into()));
        }
        3 => {}
        _ => {
            a.insert(
                "title".into(),
                Value::Str(format!("quarterly running n{}", i % 3)),
            );
        }
    }
    match i % 3 {
        0 => {
            a.insert("tags".into(), strs(&["Rust", "db"]));
        }
        1 => {
            a.insert("tags".into(), Value::Str("rust Search".into()));
        }
        _ => {}
    }
    d
}

/// The writers: `text`'s (it writes `multi` and `text`'s twin), `title`'s and `tags`'.
struct World {
    store: Arc<MemoryStore>,
    a: Engine<MemoryStore>,
    b: Engine<MemoryStore>,
    c: Engine<MemoryStore>,
}

const T: TenantId = TenantId(15200);

impl World {
    fn new(t: TenantId) -> Self {
        let store = Arc::new(MemoryStore::new());
        let e = |lane| Engine::new(Arc::clone(&store), t, LaneId(lane));
        Self {
            a: e(1),
            b: e(2).with_text_field("title"),
            c: e(3).with_text_field("tags"),
            store,
        }
    }

    /// The same rows to `multi` and to each twin.
    async fn write(&self, rows: impl Iterator<Item = u32> + Clone) {
        let docs = || rows.clone().map(row).collect::<Vec<_>>();
        self.a
            .write_declared(
                "multi",
                docs(),
                Metric::DotProduct,
                None,
                &declaring(further()),
            )
            .await
            .unwrap();
        self.a.write("twin_text", docs()).await.unwrap();
        self.b
            .write_with("twin_title", docs(), Metric::DotProduct, &title_ft())
            .await
            .unwrap();
        self.c
            .write_with("twin_tags", docs(), Metric::DotProduct, &tags_ft())
            .await
            .unwrap();
    }

    async fn fold(&self) -> Epoch {
        for e in [&self.a, &self.b, &self.c] {
            e.flush().await.unwrap();
        }
        self.a.fold().await.unwrap()
    }

    /// Each field, its twin index, and the engine that wrote the twin.
    fn twins(&self) -> [(&'static str, &'static str, &Engine<MemoryStore>); 3] {
        [
            ("text", "twin_text", &self.a),
            ("title", "twin_title", &self.b),
            ("tags", "twin_tags", &self.c),
        ]
    }
}

fn leg(field: &str, q: &str) -> Prefetch {
    Prefetch::Text {
        field: field.to_owned(),
        query: q.to_owned(),
        limit: 10,
        last_as_prefix: false,
    }
}

/// The queries for each field: one leg, and two legs over the field.
fn queries(field: &str) -> Vec<Vec<Prefetch>> {
    let qs: &[&str] = match field {
        "text" => &["alpha", "gamma n3", "beta delta"],
        "title" => &["running report", "quarterly", "the year"],
        _ => &["Rust", "rust", "Search db"],
    };
    let mut out: Vec<Vec<Prefetch>> = qs.iter().map(|q| vec![leg(field, q)]).collect();
    out.push(vec![leg(field, qs[0]), leg(field, qs[1])]);
    out
}

/// What an answer ranks: ids in order, and scores.
fn ranked(a: &pstore_engine::Answer) -> Vec<(Option<String>, f32)> {
    a.ids
        .iter()
        .cloned()
        .zip(a.hits.iter().map(|h| h.score))
        .collect()
}

fn same(
    point: &str,
    legs: &[Prefetch],
    multi: &pstore_engine::Answer,
    twin: &pstore_engine::Answer,
) {
    let (m, t) = (ranked(multi), ranked(twin));
    assert_eq!(
        m.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        t.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        "{point}: {legs:?} ranks other rows"
    );
    // M153: to the bit, since one field is scored by the same code over the same postings.
    for ((_, x), (_, y)) in m.iter().zip(&t) {
        assert_eq!(
            x.to_bits(),
            y.to_bits(),
            "{point}: {legs:?} scores {x} against {y}"
        );
    }
}

/// Every field's queries over `multi` and over its twin, live.
async fn twins_agree(w: &World, point: &str) {
    let mut answered = 0;
    for (field, twin, engine) in w.twins() {
        for legs in queries(field) {
            let m =
                w.a.query("multi", &legs, Fusion::default(), 10)
                    .await
                    .unwrap();
            let t = engine
                .query(twin, &legs, Fusion::default(), 10)
                .await
                .unwrap();
            same(point, &legs, &m, &t);
            answered += usize::from(!m.hits.is_empty());
        }
    }
    assert!(
        answered >= 9,
        "{point}: only {answered} queries found anything"
    );
}

/// The same, `as_of` an epoch.
async fn twins_agree_at(w: &World, epoch: Epoch, point: &str) {
    for (field, twin, engine) in w.twins() {
        for legs in queries(field) {
            let m =
                w.a.query_as_of("multi", epoch, &legs, Fusion::default(), 10)
                    .await
                    .unwrap();
            let t = engine
                .query_as_of(twin, epoch, &legs, Fusion::default(), 10)
                .await
                .unwrap();
            assert!(
                !t.hits.is_empty() || field == "tags",
                "{point}: {legs:?} empty"
            );
            same(point, &legs, &m, &t);
        }
    }
}

/// Criteria 1 and 2: each field of `multi` scores as its twin, at every point; `multi`'s
/// schema records the fields, and its dropped schema keeps them.
#[tokio::test]
async fn each_further_field_scores_as_its_own_index_would() {
    let w = World::new(T);
    w.write(0..40).await;
    twins_agree(&w, "before the first fold").await;

    // ⚠️ One fold for every lane, so one epoch is the same point for `multi` and its twins.
    let first = w.fold().await;
    twins_agree(&w, "after the fold").await;
    let head = w.a.head_for_test().await;
    let schema = &head.schemas["multi"];
    assert_eq!(schema.texts, further());
    assert_eq!(schema.text_field, "text");
    assert_eq!(
        Head::decode(&head.encode()).unwrap().schemas["multi"],
        *schema
    );

    // Fresh rows beside a segment, some replacing folded ones.
    w.write(30..60).await;
    twins_agree(&w, "beside a segment").await;
    w.fold().await;
    twins_agree(&w, "after a second fold").await;

    for index in ["multi", "twin_text", "twin_title", "twin_tags"] {
        assert!(
            w.a.compact(index).await.unwrap().is_some(),
            "{index} not merged"
        );
    }
    assert_eq!(w.a.head_for_test().await.indexes["multi"].len(), 1);
    twins_agree(&w, "after a compaction").await;
    twins_agree_at(&w, first, "as_of the first fold").await;

    // Dropped and re-created with another set: the dropped schema keeps the first.
    w.a.delete_index("multi").await.unwrap().unwrap();
    let other = BTreeMap::from([("tags".to_owned(), tags_ft())]);
    w.a.write_declared(
        "multi",
        vec![row(1)],
        Metric::DotProduct,
        None,
        &declaring(other.clone()),
    )
    .await
    .unwrap();
    w.fold().await;
    let head = Head::decode(&w.a.head_for_test().await.encode()).unwrap();
    assert_eq!(head.schemas["multi"].texts, other);
    let dropped = head.dropped.iter().find(|(n, _, _)| n == "multi").unwrap();
    assert_eq!(dropped.2.texts, further());
    twins_agree_at(&w, first, "as_of a dropped index").await;
}

fn is_conflict(r: &Result<(), EngineError>, what: &str) -> bool {
    matches!(r, Err(EngineError::SchemaConflict { what: w, .. }) if *w == what)
}

/// Criterion 3: a different set is refused, before and after the first fold; each door
/// refusal; and a row declaring nothing has no opinion.
#[tokio::test]
async fn what_further_fields_cannot_mean_is_refused() {
    let w = World::new(TenantId(15201));
    let write = |index: &'static str, texts: BTreeMap<String, FullText>| {
        let a = &w.a;
        async move {
            a.write_declared(
                index,
                vec![row(1), row(2)],
                Metric::DotProduct,
                None,
                &declaring(texts),
            )
            .await
        }
    };
    write("multi", further()).await.unwrap();
    // Before any fold, this process's own unfolded rows decide.
    let title_only: BTreeMap<String, FullText> = further()
        .into_iter()
        .filter(|(n, _)| n == "title")
        .collect();
    assert!(is_conflict(
        &write("multi", title_only.clone()).await,
        "the further text fields"
    ));
    w.fold().await;

    let mut added = further();
    added.insert("body".to_owned(), FullText::default());
    let mut changed = further();
    changed.insert(
        "title".to_owned(),
        FullText {
            k1: 1.0,
            ..title_ft()
        },
    );
    for (why, texts) in [
        ("added", added),
        ("removed", title_only),
        ("changed", changed),
    ] {
        assert!(
            is_conflict(&write("multi", texts).await, "the further text fields"),
            "a name {why} was accepted"
        );
    }
    w.a.write("multi", vec![row(3)]).await.unwrap();

    // The fold that creates an index records what its rows declared, even when a later,
    // undeclared write of the same id supersedes them in that fold.
    write("superseded", further()).await.unwrap();
    w.a.write("superseded", vec![row(1), row(2)]).await.unwrap();
    w.fold().await;
    assert_eq!(
        w.a.head_for_test().await.schemas["superseded"].texts,
        further()
    );

    let refused = |names: Vec<String>| {
        names
            .into_iter()
            .map(|n| (n, FullText::default()))
            .collect::<BTreeMap<_, _>>()
    };
    for (why, names) in [
        ("nine names", (0..9).map(|i| format!("f{i}")).collect()),
        ("an empty name", vec![String::new()]),
        ("a long name", vec!["x".repeat(129)]),
        ("a reserved name", vec!["$x".to_owned()]),
        ("id", vec!["id".to_owned()]),
        ("the writer's own field", vec!["text".to_owned()]),
    ] {
        let r = write("fresh", refused(names)).await;
        assert!(matches!(r, Err(EngineError::Refused(_))), "{why}: {r:?}");
    }
    // Eight names, each 128 bytes, are accepted.
    let eight: Vec<String> = (0..8).map(|i| format!("{i}{}", "y".repeat(127))).collect();
    write("fresh", refused(eight)).await.unwrap();
}

/// Criterion 3, no name is both: a writer over `title` never fills the primary of an index
/// that declares `title`, through a row or through a patch, and its own engine answers a
/// query over `title`.
#[tokio::test]
async fn a_further_field_is_never_the_primary() {
    let w = World::new(TenantId(15202));
    // The declaring writer's rows carry no primary text.
    let plain = |i: u32| {
        let mut d = row(i);
        d.attrs.remove("text");
        d
    };
    let titled = |i: u32| {
        let mut d = plain(i);
        d.attrs.insert(
            "title".into(),
            Value::Str(format!("quarterly zebras unique{i}")),
        );
        d
    };
    // Two writers over `title`: one row folded beside the declaring rows, and one left
    // unflushed by a writer whose memtable the fold does not touch.
    let d = Engine::new(Arc::clone(&w.store), TenantId(15202), LaneId(4)).with_text_field("title");
    d.write("multi", vec![titled(100)]).await.unwrap();
    d.flush().await.unwrap();
    w.b.write("multi", vec![titled(101)]).await.unwrap();
    // ⚠️ Its fresh view, built before any schema records the further fields (M152): the fold
    // below must rebuild it, though its rows are unchanged.
    let q = vec![leg("title", "zebra")];
    w.b.query("multi", &q, Fusion::default(), 10).await.unwrap();
    w.a.write_declared(
        "multi",
        (0..20).map(plain).collect(),
        Metric::DotProduct,
        None,
        &declaring(further()),
    )
    .await
    .unwrap();
    w.a.flush().await.unwrap();
    w.a.fold().await.unwrap();
    let head = w.a.head_for_test().await;
    assert_eq!(
        head.schemas["multi"].text_field, "",
        "the primary was filled"
    );
    assert_eq!(
        head.schema_rejects.get("multi"),
        Some(&1),
        "the title row was not quarantined"
    );

    // Its own unfolded row is in its fresh view, under `title` alone: stemmed by `title`'s
    // analyzer, which a view built before the fold did not know.
    let got = w.b.query("multi", &q, Fusion::default(), 10).await.unwrap();
    assert_eq!(got.ids.first().cloned().flatten().as_deref(), Some("d0101"));

    // Its flush refuses that row once, then waives it to the fold's quarantine (M35).
    let r = w.b.flush().await.map(|_| ());
    assert!(is_conflict(&r, "the text field"), "{r:?}");
    // After the fold, its rows are refused at the door.
    let r = w.b.write("multi", vec![titled(102)]).await;
    assert!(is_conflict(&r, "the text field"), "{r:?}");

    // A patch is rowless: it sets `title` text, and the primary stays empty.
    w.b.patch(
        "multi",
        vec![Patch {
            id: "d0003".to_owned(),
            set: BTreeMap::from([("title".to_owned(), Value::Str("patched headline".into()))]),
            unset: Vec::new(),
        }],
        None,
    )
    .await
    .unwrap();
    w.fold().await;
    let head = w.a.head_for_test().await;
    assert_eq!(
        head.schemas["multi"].text_field, "",
        "a patch filled the primary"
    );
    for e in [&w.a, &w.b] {
        let got = e
            .query(
                "multi",
                &[leg("title", "patched headline")],
                Fusion::default(),
                10,
            )
            .await
            .unwrap();
        assert_eq!(got.ids[0].as_deref(), Some("d0003"));
    }
}

/// Each leg's own ranking over its field's twin, as `(id, score)` by rank: unlimited, so a
/// `Sum` or `Max` finds every leg score, and cut at the leg's limit for RRF.
async fn own_ranking(w: &World, leg: &Prefetch, epoch: Option<Epoch>) -> Vec<(String, f32)> {
    let Prefetch::Text { field, .. } = leg else {
        panic!("a text leg");
    };
    let (_, twin, engine) = w.twins().into_iter().find(|(f, _, _)| f == field).unwrap();
    let one = Fusion::Sum {
        weights: Weights::of(&[1.0]).unwrap(),
    };
    let legs = std::slice::from_ref(leg);
    let a = match epoch {
        None => engine.query(twin, legs, one, 500).await.unwrap(),
        Some(e) => engine.query_as_of(twin, e, legs, one, 500).await.unwrap(),
    };
    ranked(&a)
        .into_iter()
        .map(|(id, score)| (id.unwrap(), score))
        .collect()
}

const LIMIT: usize = 12;

fn fleg(field: &str, q: &str) -> Prefetch {
    Prefetch::Text {
        field: field.to_owned(),
        query: q.to_owned(),
        limit: LIMIT,
        last_as_prefix: false,
    }
}

/// Criterion 1 (M153): a query over several fields of `multi` fuses each leg's own score --
/// its twin's, over that field alone -- by `Sum`, `Max` and RRF, at every point.
async fn several_agree(w: &World, epoch: Option<Epoch>, point: &str) {
    let top_k = 10;
    let pairs = [
        vec![fleg("title", "running report"), fleg("text", "alpha gamma")],
        vec![
            fleg("text", "beta n3"),
            fleg("title", "quarterly"),
            fleg("tags", "Rust"),
        ],
    ];
    for legs in &pairs {
        let n = legs.len();
        let ws: Vec<f32> = (0..n).map(|i| 1.0 / (i as f32 + 1.0)).collect();
        let weights = Weights::of(&ws).unwrap();
        let mut own = Vec::new();
        for leg in legs {
            own.push(own_ranking(w, leg, epoch).await);
        }
        for (name, fusion) in [
            ("sum", Fusion::Sum { weights }),
            ("max", Fusion::Max { weights }),
            ("rrf", Fusion::Rrf { k: 60.0 }),
        ] {
            let mut want: BTreeMap<String, f32> = BTreeMap::new();
            for (j, ranking) in own.iter().enumerate() {
                let cut = if name == "sum" { ranking.len() } else { LIMIT };
                for (rank, (id, score)) in ranking.iter().take(cut).enumerate() {
                    let v = match name {
                        "rrf" => 1.0 / (60.0 + (rank + 1) as f32),
                        _ => ws[j] * score,
                    };
                    let e = want.entry(id.clone()).or_insert(0.0);
                    *e = if name == "max" { e.max(v) } else { *e + v };
                }
            }
            let got = match epoch {
                None => w.a.query("multi", legs, fusion, top_k).await.unwrap(),
                Some(e) => {
                    w.a.query_as_of("multi", e, legs, fusion, top_k)
                        .await
                        .unwrap()
                }
            };
            let got = ranked(&got);
            let mut best: Vec<f32> = want.values().copied().collect();
            best.sort_by(|a, b| b.total_cmp(a));
            let what = format!("{point}: {name} over {legs:?}");
            assert_eq!(got.len(), top_k.min(best.len()), "{what}");
            let floor = best[got.len() - 1];
            for (k, (id, score)) in got.iter().enumerate() {
                let id = id.as_deref().unwrap();
                let w = want[id];
                assert!(
                    (score - w).abs() <= 1e-5,
                    "{what}: {id} scored {score}, not {w}"
                );
                assert!(
                    w >= floor - 1e-5,
                    "{what}: {id} ({w}) is not in the top {top_k}"
                );
                if let Some((_, next)) = got.get(k + 1) {
                    assert!(score >= next, "{what}: not in order");
                }
            }
        }
    }
}

#[tokio::test]
async fn several_fields_fuse_as_their_own_rankings_do() {
    let w = World::new(TenantId(15203));
    w.write(0..40).await;
    several_agree(&w, None, "before the first fold").await;
    let first = w.fold().await;
    several_agree(&w, None, "after the fold").await;
    w.write(30..60).await;
    several_agree(&w, None, "beside a segment").await;
    w.fold().await;
    for index in ["multi", "twin_text", "twin_title", "twin_tags"] {
        w.a.compact(index).await.unwrap();
    }
    several_agree(&w, None, "after a compaction").await;
    several_agree(&w, Some(first), "as_of the first fold").await;
}

/// Criterion 3 (M153), through the engine: a query naming a field no segment carries, beside
/// one that is, is the segment's unknown field -- not an answer that leaves its legs out.
#[tokio::test]
async fn an_unknown_field_beside_a_known_one_is_refused() {
    let w = World::new(TenantId(15303));
    w.write(0..20).await;
    w.fold().await;
    let legs = [fleg("title", "quarterly"), fleg("nosuch", "quarterly")];
    let sum = Fusion::Sum {
        weights: Weights::of(&[1.0, 1.0]).unwrap(),
    };
    let e = w.a.query("multi", &legs, sum, 10).await.unwrap_err();
    // M155: typed, and naming the field no segment holds.
    assert!(
        matches!(e, EngineError::UnknownTextField(ref f) if f == "nosuch"),
        "{e:?}"
    );
}
