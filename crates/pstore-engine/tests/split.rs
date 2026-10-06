//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M54: a query whose vector legs run on other servers answers exactly as one that runs them
//! all here -- hit for hit, in score bits -- for what the API cannot ask: sparse legs, two
//! dense legs, a shadow of unfolded writes, deleted rows and a filter. Each "server" is an
//! engine over the same store, and a share is run through [`Engine::part`], the call the
//! server's endpoint makes.

use pstore_blob::MemoryStore;
use pstore_engine::{Engine, Part, Peers, Phased};
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_query::{Fusion, Op, PartHits, Predicate, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const T: TenantId = TenantId(5400);

fn engine(store: &Arc<MemoryStore>, lane: u64) -> Engine<MemoryStore> {
    Engine::new(Arc::clone(store), T, LaneId(lane)).with_index_params(
        pstore_index::cluster::Params {
            // Every segment clustered, so the dense leg reads a centroid table.
            exact_scan_threshold: 8,
            ..pstore_index::cluster::Params::default()
        },
    )
}

fn doc(i: u32) -> Document {
    let x = i as f32;
    let mut d = Document::new(
        format!("d{i:05}"),
        vec![x.sin(), x.cos(), (x * 0.37).sin(), 1.0],
    );
    d.attrs.insert("n".to_owned(), Value::Int(i64::from(i)));
    d.attrs.insert(
        "text".to_owned(),
        Value::Str(format!("common word{} tag{}", i % 5, i % 3)),
    );
    d.vectors.insert(
        "v2".to_owned(),
        VectorField::Dense(vec![vec![(x * 0.11).cos(), (x * 0.7).sin(), 0.5]]),
    );
    d.vectors.insert(
        "s".to_owned(),
        VectorField::Sparse(vec![
            (3, Impact::new(0.5 + (i % 4) as f32 * 0.1)),
            (i % 7 + 10, Impact::new(0.25)),
        ]),
    );
    d
}

/// Three servers by name; this one is the first. Each share runs on its server's engine, and
/// is counted -- a phased one per phase, with the text legs it carried (M55).
struct Three {
    names: Vec<String>,
    engines: Vec<Arc<Engine<MemoryStore>>>,
    filter: Option<Predicate>,
    parts: Arc<AtomicU64>,
    opens: Arc<AtomicU64>,
    scans: Arc<AtomicU64>,
    text_legs: Arc<AtomicU64>,
    /// The most `df` entries any phase 2 was sent that are not the query's terms (code
    /// review): the sum must carry the query's terms only, never the coordinator's vocabulary.
    stray: Arc<AtomicU64>,
    /// Every part opened in phase 1: its segments, and what `OpenPart::bytes` says it holds.
    held: Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    /// The `sum` cut each phased share carried, in the order they were made (M58).
    cuts: Arc<std::sync::Mutex<Vec<Option<usize>>>>,
    /// The most distinct rows any phase 2 sent back (M58).
    most_rows: Arc<AtomicU64>,
    /// Every segment ordinal a phased share was given (M58).
    given: Arc<std::sync::Mutex<std::collections::BTreeSet<usize>>>,
    /// Which phase fails, if any: 1 or 2.
    fails: u8,
}

impl Three {
    fn new(engines: Vec<Arc<Engine<MemoryStore>>>, filter: Option<Predicate>) -> Self {
        Self {
            names: vec!["http://a".into(), "http://b".into(), "http://c".into()],
            engines,
            filter,
            parts: Arc::default(),
            opens: Arc::default(),
            scans: Arc::default(),
            text_legs: Arc::default(),
            stray: Arc::default(),
            held: Arc::default(),
            cuts: Arc::default(),
            most_rows: Arc::default(),
            given: Arc::default(),
            fails: 0,
        }
    }
}

impl Peers for Three {
    fn servers(&self) -> &[String] {
        &self.names
    }
    fn me(&self) -> usize {
        0
    }
    fn part(
        &self,
        server: usize,
        part: Part,
    ) -> futures_util::future::BoxFuture<'static, Result<PartHits, String>> {
        let engine = Arc::clone(&self.engines[server]);
        let filter = self.filter.clone();
        let parts = Arc::clone(&self.parts);
        Box::pin(async move {
            parts.fetch_add(1, Ordering::SeqCst);
            engine
                .part(&part, filter.as_ref())
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn phased(&self, server: usize, part: Part) -> Phased {
        let engine = Arc::clone(&self.engines[server]);
        let filter = self.filter.clone();
        let (opens, scans, fails) = (Arc::clone(&self.opens), Arc::clone(&self.scans), self.fails);
        let stray = Arc::clone(&self.stray);
        let sizes = Arc::clone(&self.held);
        self.cuts.lock().unwrap().push(part.sum.map(|c| c.keep));
        self.given
            .lock()
            .unwrap()
            .extend(part.targets.iter().map(|(i, _)| *i));
        let most_rows = Arc::clone(&self.most_rows);
        let text = part
            .legs
            .iter()
            .filter(|(_, p)| matches!(p, Prefetch::Text { .. }))
            .count() as u64;
        self.text_legs.fetch_add(text, Ordering::SeqCst);
        let held: Arc<std::sync::Mutex<Option<pstore_query::OpenPart>>> = Arc::default();
        let part = Arc::new(part);
        let stats = {
            let (engine, held, part) = (Arc::clone(&engine), Arc::clone(&held), Arc::clone(&part));
            Box::pin(async move {
                opens.fetch_add(1, Ordering::SeqCst);
                if fails == 1 {
                    return Err("phase 1 down".to_owned());
                }
                let (open, stats) = engine.open_part(&part).await.map_err(|e| e.to_string())?;
                sizes
                    .lock()
                    .unwrap()
                    .push((part.targets.len(), open.bytes()));
                *held.lock().unwrap() = Some(open);
                Ok(stats)
            }) as futures_util::future::BoxFuture<'static, _>
        };
        let scan = Box::new(move |sum: pstore_index::text::Stats| {
            Box::pin(async move {
                scans.fetch_add(1, Ordering::SeqCst);
                let n = sum.df.keys().filter(|t| !part.terms.contains(*t)).count() as u64;
                stray.fetch_max(n, Ordering::SeqCst);
                if fails == 2 {
                    return Err("lost between phases".to_owned());
                }
                let open = held.lock().unwrap().take().ok_or("nothing held")?;
                let hits = engine
                    .scan_part(&part, &open, filter.as_ref(), &sum)
                    .await
                    .map_err(|e| e.to_string())?;
                most_rows.fetch_max(rows_of(&hits) as u64, Ordering::SeqCst);
                Ok(hits)
            }) as futures_util::future::BoxFuture<'static, _>
        });
        Phased { stats, scan }
    }
}

/// Everything an answer says, floats by their bits.
fn exactly(a: &pstore_engine::Answer) -> String {
    let hits: Vec<(usize, usize, u32)> = a
        .hits
        .iter()
        .map(|h| (h.segment, h.row, h.score.to_bits()))
        .collect();
    let dists: Vec<Option<u32>> = a.dists.iter().map(|d| d.map(f32::to_bits)).collect();
    format!("{hits:?} {:?} {:?} {dists:?}", a.ids, a.attributes)
}

fn dense(field: &str, q: Vec<f32>) -> Prefetch {
    Prefetch::Dense {
        field: field.to_owned(),
        query: q,
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }
}

fn sparse() -> Prefetch {
    Prefetch::Sparse {
        field: "s".to_owned(),
        query: vec![(3, 1.0), (12, 0.5)],
        limit: 10,
    }
}

fn text() -> Prefetch {
    Prefetch::Text {
        field: "text".to_owned(),
        query: "word2 tag1".to_owned(),
        limit: 10,
    }
}

#[tokio::test]
async fn a_split_query_equals_the_unsplit_one() {
    let store = Arc::new(MemoryStore::new());
    let coordinator = engine(&store, 1);
    // Eight folded segments of 24 rows.
    for k in 0..8u32 {
        coordinator
            .write("idx", (k * 24..k * 24 + 24).map(doc).collect())
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        coordinator.fold().await.unwrap();
    }
    // Deleted rows, folded: delete vectors.
    coordinator
        .delete(
            "idx",
            (0..192).step_by(9).map(|i| format!("d{i:05}")).collect(),
        )
        .await
        .unwrap();
    coordinator.flush().await.unwrap();
    coordinator.fold().await.unwrap();
    // Unfolded upserts and deletes: a shadow over every folded segment.
    coordinator
        .write("idx", (0..192).step_by(7).map(|i| doc(i + 1_000)).collect())
        .await
        .unwrap();
    coordinator
        .write(
            "idx",
            (0..192)
                .step_by(5)
                .map(|i| {
                    let mut d = doc(i);
                    d.attrs.insert("n".to_owned(), Value::Int(-1));
                    d
                })
                .collect(),
        )
        .await
        .unwrap();
    coordinator
        .delete(
            "idx",
            (1..192).step_by(11).map(|i| format!("d{i:05}")).collect(),
        )
        .await
        .unwrap();

    let q = vec![0.3, 0.9, -0.2, 1.0];
    let filtered = Predicate::Cmp("n".to_owned(), Op::Gt, Value::Int(40));
    let cases: Vec<(&str, Vec<Prefetch>, Option<Predicate>, Fusion)> = vec![
        (
            "dense",
            vec![dense("vector", q.clone())],
            None,
            Fusion::default(),
        ),
        (
            "dense, filtered",
            vec![dense("vector", q.clone())],
            Some(filtered.clone()),
            Fusion::default(),
        ),
        ("sparse", vec![sparse()], None, Fusion::default()),
        (
            "two dense legs",
            vec![
                dense("vector", q.clone()),
                dense("v2", vec![0.2, -0.4, 0.5]),
            ],
            None,
            Fusion::default(),
        ),
        (
            "dense, sparse and text, filtered",
            vec![dense("vector", q.clone()), sparse(), text()],
            Some(filtered.clone()),
            Fusion::default(),
        ),
    ];
    let store_peer = |lane| Arc::new(engine(&store, lane));
    for (name, legs, filter, fusion) in cases {
        let unsplit = coordinator
            .query_filtered("idx", &legs, filter.as_ref(), fusion, 10)
            .await
            .unwrap();
        let peers = Three::new(
            vec![store_peer(10), store_peer(11), store_peer(12)],
            filter.clone(),
        );
        let split = coordinator
            .query_split_as(
                "idx",
                &legs,
                filter.as_ref(),
                fusion,
                10,
                pstore_engine::Consistency::Eventual,
                Some(&peers),
            )
            .await
            .unwrap();
        assert!(!unsplit.hits.is_empty(), "{name}: nothing to compare");
        // A share in one exchange, or -- with a text leg, M55 -- in two.
        assert_eq!(
            peers.parts.load(Ordering::SeqCst) + peers.opens.load(Ordering::SeqCst),
            2,
            "{name}: not split"
        );
        assert_eq!(exactly(&split), exactly(&unsplit), "{name}");
    }
}

#[tokio::test]
async fn a_share_that_fails_is_run_here() {
    // M54 rule 5: an error from a peer costs rounds, never the answer.
    struct Failing(Vec<String>, Arc<AtomicU64>);
    impl Peers for Failing {
        fn servers(&self) -> &[String] {
            &self.0
        }
        fn me(&self) -> usize {
            0
        }
        fn part(
            &self,
            _: usize,
            _: Part,
        ) -> futures_util::future::BoxFuture<'static, Result<PartHits, String>> {
            let n = Arc::clone(&self.1);
            Box::pin(async move {
                n.fetch_add(1, Ordering::SeqCst);
                Err("down".to_owned())
            })
        }
        fn phased(&self, _: usize, _: Part) -> Phased {
            Phased {
                stats: Box::pin(async { Err("down".to_owned()) }),
                scan: Box::new(|_| Box::pin(async { Err("down".to_owned()) })),
            }
        }
    }
    let store = Arc::new(MemoryStore::new());
    let w = engine(&store, 1);
    for k in 0..8u32 {
        w.write("idx", (k * 24..k * 24 + 24).map(doc).collect())
            .await
            .unwrap();
        w.flush().await.unwrap();
        w.fold().await.unwrap();
    }
    let legs = vec![dense("vector", vec![0.3, 0.9, -0.2, 1.0])];
    let unsplit = w.query("idx", &legs, Fusion::default(), 10).await.unwrap();
    let peers = Failing(
        vec!["http://a".into(), "http://b".into(), "http://c".into()],
        Arc::new(AtomicU64::new(0)),
    );
    let split = w
        .query_split_as(
            "idx",
            &legs,
            None,
            Fusion::default(),
            10,
            pstore_engine::Consistency::Eventual,
            Some(&peers),
        )
        .await
        .unwrap();
    assert_eq!(peers.1.load(Ordering::SeqCst), 2);
    assert_eq!(exactly(&split), exactly(&unsplit));
}

#[test]
fn assignment_is_stable_balanced_and_moves_only_to_a_new_server() {
    use pstore_engine::assign;
    let four: Vec<String> = (0..4).map(|i| format!("http://10.0.0.{i}:8080")).collect();
    let keys: Vec<String> = (0..10_000)
        .map(|i| format!("0001/tnt/1/idx/docs/seg/L0/{i:020}-0000000000000001.seg"))
        .collect();
    // The same server whatever the list order, and with a trailing `/`.
    let mut shuffled = four.clone();
    shuffled.reverse();
    shuffled[1].push('/');
    for k in &keys[..500] {
        let a = &four[assign(k, &four)];
        let b = shuffled[assign(k, &shuffled)].trim_end_matches('/');
        assert_eq!(a, b, "{k}");
    }
    let mut held = [0usize; 4];
    for k in &keys {
        held[assign(k, &four)] += 1;
    }
    for (i, n) in held.iter().enumerate() {
        assert!(
            (2_200..=2_800).contains(n),
            "server {i} holds {n} of 10,000"
        );
    }
    let mut five = four.clone();
    five.push("http://10.0.0.4:8080".to_owned());
    let mut moved = 0;
    for k in &keys {
        let (before, after) = (assign(k, &four), assign(k, &five));
        if before != after {
            assert_eq!(after, 4, "{k} moved between old servers");
            moved += 1;
        }
    }
    assert!((1_700..=2_300).contains(&moved), "{moved} of 10,000 moved");
    assert_eq!(assign("x", &[]), 0);
    // Pinned against an independent model (Python, in the ledger): every coordinator of every
    // build must agree, so the hash is part of the protocol. The sweep found the finaliser's
    // mutants alive -- a weaker mix is still stable and roughly balanced.
    let pinned: Vec<usize> = keys[..16].iter().map(|k| assign(k, &four)).collect();
    assert_eq!(pinned, [2, 3, 3, 3, 3, 1, 0, 3, 3, 0, 0, 2, 1, 3, 2, 3]);
}

// ---- M55: text legs split too ------------------------------------------------------------

/// Counts the term-dictionary reads of one engine's view of a shared store.
#[derive(Debug, Clone)]
struct Tdicts {
    inner: MemoryStore,
    read: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Tdicts {
    fn saw(&self, key: &pstore_blob::Key) {
        self.read.lock().unwrap().push(key.as_str().to_owned());
    }
}

#[async_trait::async_trait]
impl pstore_blob::BlobStore for Tdicts {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &pstore_blob::Key) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.saw(key);
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &pstore_blob::Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.saw(key);
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(
        &self,
        key: &pstore_blob::Key,
        ranges: &[std::ops::Range<u64>],
    ) -> Result<Vec<bytes::Bytes>, pstore_blob::BlobError> {
        self.saw(key);
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(
        &self,
        key: &pstore_blob::Key,
        n: u64,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.saw(key);
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(
        &self,
        key: &pstore_blob::Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        self.saw(key);
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
}

/// A row whose text a share's statistics differ on: `alpha` is common in the first two
/// segments and absent after, so a peer scoring with its own `df` ranks differently.
fn tdoc(i: u32, with_text: bool) -> Document {
    let mut d = doc(i);
    if with_text {
        let alpha = if i < 48 { "alpha alpha" } else { "" };
        d.attrs.insert(
            "text".to_owned(),
            Value::Str(format!(
                "{alpha} common word{} tag{} n{}",
                i % 5,
                i % 3,
                i % 11
            )),
        );
    } else {
        d.attrs.remove("text");
    }
    d
}

fn tengine(store: &Tdicts, lane: u64) -> Engine<Tdicts> {
    Engine::new(Arc::new(store.clone()), T, LaneId(lane)).with_index_params(
        pstore_index::cluster::Params {
            exact_scan_threshold: 8,
            ..pstore_index::cluster::Params::default()
        },
    )
}

/// Eight folded segments of 24 rows in `index`, the first `texts` of them with text; then
/// unfolded writes whose text holds `zeta`, a term only the fresh segment has.
async fn tfill(w: &Engine<Tdicts>, index: &str, texts: u32, unfolded: bool) {
    for k in 0..8u32 {
        w.write(
            index,
            (k * 24..k * 24 + 24).map(|i| tdoc(i, k < texts)).collect(),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        w.fold().await.unwrap();
    }
    w.delete(
        index,
        (0..192).step_by(9).map(|i| format!("d{i:05}")).collect(),
    )
    .await
    .unwrap();
    w.flush().await.unwrap();
    w.fold().await.unwrap();
    if unfolded {
        let fresh: Vec<Document> = (0..192)
            .step_by(7)
            .map(|i| {
                let mut d = tdoc(i + 1_000, true);
                d.attrs.insert(
                    "text".to_owned(),
                    Value::Str(format!("zeta common word{}", i % 5)),
                );
                d
            })
            .collect();
        w.write(index, fresh).await.unwrap();
    }
}

fn tquery(q: &str) -> Prefetch {
    Prefetch::Text {
        field: "text".to_owned(),
        query: q.to_owned(),
        limit: 10,
    }
}

/// `Engine<MemoryStore>` peers over the store a `Tdicts` coordinator writes through.
fn peers_of(t: &Tdicts, filter: Option<Predicate>) -> Three {
    let e = |lane| {
        Arc::new(
            Engine::new(Arc::new(t.inner.clone()), T, LaneId(lane)).with_index_params(
                pstore_index::cluster::Params {
                    exact_scan_threshold: 8,
                    ..pstore_index::cluster::Params::default()
                },
            ),
        )
    };
    Three::new(vec![e(10), e(11), e(12)], filter)
}

/// One case: its name, index, coordinator, legs, filter, fusion and consistency.
type Case<'a> = (
    &'static str,
    &'static str,
    &'a Engine<Tdicts>,
    Vec<Prefetch>,
    Option<Predicate>,
    Fusion,
    pstore_engine::Consistency,
);

#[tokio::test]
async fn a_split_text_query_equals_the_unsplit_one() {
    let store = Tdicts {
        inner: MemoryStore::new(),
        read: Arc::default(),
    };
    let w = tengine(&store, 1);
    tfill(&w, "idx", 8, true).await;
    // An index whose folded segments have no text: only the fresh segment does.
    tfill(&w, "fresh", 0, true).await;
    // A second writer's index with nothing unfolded, for `strong`.
    let other = tengine(&store, 2);
    tfill(&other, "folded", 8, false).await;
    // Text in the first segment only: at least one peer's share has none.
    tfill(&w, "mixed", 1, false).await;

    let q = vec![0.3, 0.9, -0.2, 1.0];
    let filtered = Predicate::Cmp("n".to_owned(), Op::Gt, Value::Int(40));
    let max = Fusion::Max {
        weights: pstore_query::Weights::ONE,
    };
    let rrf = Fusion::default();
    let eventual = pstore_engine::Consistency::Eventual;
    let cases: Vec<Case<'_>> = vec![
        (
            "text",
            "idx",
            &w,
            vec![tquery("alpha word2 zeta")],
            None,
            rrf,
            eventual,
        ),
        (
            "text, filtered",
            "idx",
            &w,
            vec![tquery("alpha tag1 zeta")],
            Some(filtered.clone()),
            rrf,
            eventual,
        ),
        (
            "max",
            "idx",
            &w,
            vec![tquery("alpha zeta"), tquery("word3 n4")],
            None,
            max,
            eventual,
        ),
        (
            "dense and text",
            "idx",
            &w,
            vec![dense("vector", q.clone()), tquery("alpha word2")],
            None,
            rrf,
            eventual,
        ),
        (
            "dense, sparse and text, filtered",
            "idx",
            &w,
            vec![
                dense("vector", q.clone()),
                sparse(),
                tquery("alpha zeta n3"),
            ],
            Some(filtered.clone()),
            rrf,
            eventual,
        ),
        (
            "an absent term",
            "idx",
            &w,
            vec![tquery("nowhere")],
            None,
            rrf,
            eventual,
        ),
        (
            "only the fresh segment has text",
            "fresh",
            &w,
            vec![tquery("zeta word1")],
            None,
            rrf,
            eventual,
        ),
        (
            "a share with no text",
            "mixed",
            &w,
            vec![dense("vector", q.clone()), tquery("alpha word2")],
            None,
            rrf,
            eventual,
        ),
        (
            "strong",
            "folded",
            &other,
            vec![tquery("alpha word2")],
            None,
            rrf,
            pstore_engine::Consistency::Strong,
        ),
    ];
    for (name, index, coord, legs, filter, fusion, consistency) in cases {
        let unsplit = coord
            .query_filtered_as(index, &legs, filter.as_ref(), fusion, 10, consistency)
            .await
            .unwrap();
        let peers = peers_of(&store, filter.clone());
        store.read.lock().unwrap().clear();
        let split = coord
            .query_split_as(
                index,
                &legs,
                filter.as_ref(),
                fusion,
                10,
                consistency,
                Some(&peers),
            )
            .await
            .unwrap();
        assert_eq!(exactly(&split), exactly(&unsplit), "{name}");
        let opens = peers.opens.load(Ordering::SeqCst);
        assert!(opens >= 1, "{name}: not split");
        assert_eq!(
            peers.scans.load(Ordering::SeqCst),
            opens,
            "{name}: a scan per open"
        );
        assert_eq!(
            peers.parts.load(Ordering::SeqCst),
            0,
            "{name}: no one-exchange part"
        );
        assert!(
            peers.text_legs.load(Ordering::SeqCst) >= opens,
            "{name}: text legs sent"
        );
        assert_eq!(
            peers.stray.load(Ordering::SeqCst),
            0,
            "{name}: phase 2 sent terms the query does not have"
        );
        // What a held part is budgeted at: 16 KiB a segment and its sidecars on top -- never
        // less, and never as if it held a whole store.
        for (n, bytes) in peers.held.lock().unwrap().iter() {
            assert!(
                *n > 0 && *bytes >= 16 * 1024 * n && *bytes < 16 * 1024 * n + (1 << 20),
                "{name}: {n} segments held as {bytes} bytes"
            );
        }
        // The coordinator read no term dictionary of a segment it gave away.
        let names: Vec<String> = peers.names.clone();
        let read = store.read.lock().unwrap().clone();
        for k in read.iter().filter(|k| k.ends_with(".tdict")) {
            let seg = k.trim_end_matches(".tdict");
            assert_eq!(
                pstore_engine::assign(seg, &names),
                0,
                "{name}: the coordinator read {k}, another server's"
            );
        }
    }
}

#[tokio::test]
async fn a_failed_text_share_costs_rounds_not_answers() {
    let store = Tdicts {
        inner: MemoryStore::new(),
        read: Arc::default(),
    };
    let w = tengine(&store, 1);
    tfill(&w, "idx", 8, true).await;
    let legs = vec![
        dense("vector", vec![0.3, 0.9, -0.2, 1.0]),
        tquery("alpha word2 zeta"),
    ];
    let unsplit = w
        .query_filtered("idx", &legs, None, Fusion::default(), 10)
        .await
        .unwrap();
    for fails in [1u8, 2] {
        let mut peers = peers_of(&store, None);
        peers.fails = fails;
        let split = w
            .query_split_as(
                "idx",
                &legs,
                None,
                Fusion::default(),
                10,
                pstore_engine::Consistency::Eventual,
                Some(&peers),
            )
            .await
            .unwrap();
        assert_eq!(exactly(&split), exactly(&unsplit), "phase {fails} failing");
        assert_eq!(peers.opens.load(Ordering::SeqCst), 2);
        assert_eq!(
            peers.scans.load(Ordering::SeqCst),
            if fails == 1 { 0 } else { 2 },
            "phase {fails}"
        );
    }
}

#[tokio::test]
async fn a_share_whose_term_dictionary_will_not_read_is_never_short() {
    // Spec review: a share whose `.tdict` fails to read must fail phase 1, never sum one
    // segment short. Here every peer's view loses the dictionaries, so every share fails and
    // the coordinator runs it -- and answers as the unsplit query does.
    #[derive(Debug, Clone)]
    struct NoTdict(MemoryStore);
    #[async_trait::async_trait]
    impl pstore_blob::BlobStore for NoTdict {
        fn capabilities(&self) -> &pstore_blob::Capabilities {
            self.0.capabilities()
        }
        async fn get(
            &self,
            key: &pstore_blob::Key,
        ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
            if key.as_str().ends_with(".tdict") {
                return Err(pstore_blob::BlobError::Other("no".into()));
            }
            self.0.get(key).await
        }
        async fn get_range(
            &self,
            key: &pstore_blob::Key,
            range: std::ops::Range<u64>,
        ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
            self.0.get_range(key, range).await
        }
        async fn get_suffix(
            &self,
            key: &pstore_blob::Key,
            n: u64,
        ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
            self.0.get_suffix(key, n).await
        }
        async fn get_with_tag(
            &self,
            key: &pstore_blob::Key,
        ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
            self.0.get_with_tag(key).await
        }
        async fn get_tag(
            &self,
            key: &pstore_blob::Key,
        ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
            self.0.get_tag(key).await
        }
        async fn head(&self, key: &pstore_blob::Key) -> Result<u64, pstore_blob::BlobError> {
            self.0.head(key).await
        }
        async fn put(
            &self,
            key: &pstore_blob::Key,
            body: bytes::Bytes,
        ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
            self.0.put(key, body).await
        }
        async fn put_conditional(
            &self,
            key: &pstore_blob::Key,
            body: bytes::Bytes,
            pre: pstore_blob::Precondition,
        ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
            self.0.put_conditional(key, body, pre).await
        }
        async fn delete_batch(
            &self,
            keys: &[pstore_blob::Key],
        ) -> Result<(), pstore_blob::BlobError> {
            self.0.delete_batch(keys).await
        }
        async fn list_unrestricted(
            &self,
            prefix: &pstore_blob::Key,
        ) -> Result<Vec<pstore_blob::Key>, pstore_blob::BlobError> {
            self.0.list_unrestricted(prefix).await
        }
    }
    struct Blind(Vec<String>, Arc<Engine<NoTdict>>, Arc<AtomicU64>);
    impl Peers for Blind {
        fn servers(&self) -> &[String] {
            &self.0
        }
        fn me(&self) -> usize {
            0
        }
        fn part(
            &self,
            _: usize,
            _: Part,
        ) -> futures_util::future::BoxFuture<'static, Result<PartHits, String>> {
            Box::pin(async { Err("unused".to_owned()) })
        }
        fn phased(&self, _: usize, part: Part) -> Phased {
            let (engine, failed) = (Arc::clone(&self.1), Arc::clone(&self.2));
            Phased {
                stats: Box::pin(async move {
                    let got = engine.open_part(&part).await.map(|(_, s)| s);
                    if got.is_err() {
                        failed.fetch_add(1, Ordering::SeqCst);
                    }
                    got.map_err(|e| e.to_string())
                }),
                scan: Box::new(|_| Box::pin(async { Err("never reached".to_owned()) })),
            }
        }
    }
    let store = Tdicts {
        inner: MemoryStore::new(),
        read: Arc::default(),
    };
    let w = tengine(&store, 1);
    tfill(&w, "idx", 8, true).await;
    let legs = vec![tquery("alpha word2 zeta")];
    let unsplit = w
        .query_filtered("idx", &legs, None, Fusion::default(), 10)
        .await
        .unwrap();
    let blind = Blind(
        vec!["http://a".into(), "http://b".into(), "http://c".into()],
        Arc::new(Engine::new(
            Arc::new(NoTdict(store.inner.clone())),
            T,
            LaneId(20),
        )),
        Arc::default(),
    );
    let split = w
        .query_split_as(
            "idx",
            &legs,
            None,
            Fusion::default(),
            10,
            pstore_engine::Consistency::Eventual,
            Some(&blind),
        )
        .await
        .unwrap();
    assert_eq!(
        blind.2.load(Ordering::SeqCst),
        2,
        "each share's phase 1 refused"
    );
    assert_eq!(exactly(&split), exactly(&unsplit));
}

#[tokio::test]
async fn a_share_sums_only_what_its_legs_and_head_ask_for() {
    // Sweep: phase 1's strictness applies to a segment HEAD says has a term dictionary, under
    // a part with a text leg -- and to nothing else, which would fail for want of a dictionary
    // it never fetched.
    let store = Tdicts {
        inner: MemoryStore::new(),
        read: Arc::default(),
    };
    let w = tengine(&store, 1);
    tfill(&w, "idx", 8, false).await;
    use pstore_blob::BlobStore as _;
    let segs: Vec<String> = store
        .inner
        .list_unrestricted(&pstore_blob::Key::new(String::new()))
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.as_str().to_owned())
        .filter(|k| k.ends_with(".seg"))
        .collect();
    assert!(segs.len() >= 8);
    let targets = |text_dict: bool| -> Vec<(usize, pstore_query::Target)> {
        segs.iter()
            .enumerate()
            .map(|(i, k)| {
                (
                    i,
                    pstore_query::Target {
                        segment: pstore_blob::Key::new(k.clone()),
                        segment_len: None,
                        centroids: None,
                        deleted: None,
                        sparse_dict: false,
                        text_dict,
                        shadowed: false,
                    },
                )
            })
            .collect()
    };
    let alpha = vec!["alpha".to_owned()];
    // No text leg: nothing summed, and no dictionary required.
    let dense_only = vec![(0, dense("vector", vec![0.3, 0.9, -0.2, 1.0]))];
    let (_, st) = pstore_query::open_part(&store.inner, &targets(true), &dense_only, &alpha)
        .await
        .unwrap();
    assert_eq!(st.doc_count, 0);
    // A text leg, over segments HEAD says have none: nothing read, nothing required.
    let text = vec![(0, tquery("alpha"))];
    let (_, st) = pstore_query::open_part(&store.inner, &targets(false), &text, &alpha)
        .await
        .unwrap();
    assert_eq!(st.doc_count, 0);
    // And over segments that have them: every one summed.
    let (_, st) = pstore_query::open_part(&store.inner, &targets(true), &text, &alpha)
        .await
        .unwrap();
    assert!(st.doc_count > 0 && st.df.contains_key("alpha"), "{st:?}");
}

/// What a `sum` share returned: the most distinct rows any one share sent back (M58 AC2).
fn rows_of(hits: &PartHits) -> usize {
    hits.iter()
        .flat_map(|(_, _, h)| h.iter().map(|x| (x.segment, x.row)))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

#[tokio::test]
async fn a_split_sum_query_equals_the_unsplit_one() {
    let store = Tdicts {
        inner: MemoryStore::new(),
        read: Arc::default(),
    };
    let w = tengine(&store, 1);
    tfill(&w, "idx", 8, false).await;
    let weights = pstore_query::Weights::of(&[2.0, 0.5]).unwrap();
    let sum = Fusion::Sum { weights };
    // A limit of 1 a leg: `sum` runs its legs whole, so the limit must change nothing -- and a
    // share run leg by leg at its limit, anywhere, loses rows the answer has.
    let one = |q: &str| Prefetch::Text {
        field: "text".to_owned(),
        query: q.to_owned(),
        limit: 1,
    };
    let legs = vec![one("alpha word2"), one("tag1 n3")];
    let eventual = pstore_engine::Consistency::Eventual;

    // The corpus has a row first by sum that neither leg ranks first alone.
    let first = |a: &pstore_engine::Answer| a.ids.first().cloned().flatten();
    let whole = w
        .query_filtered_as("idx", &legs, None, sum, 5, eventual)
        .await
        .unwrap();
    let alone: Vec<Option<String>> = futures_util::future::join_all(legs.iter().map(|l| {
        let w = &w;
        async move {
            let a = w
                .query_filtered_as("idx", std::slice::from_ref(l), None, sum, 1, eventual)
                .await
                .unwrap();
            first(&a)
        }
    }))
    .await;
    assert!(
        !alone.contains(&first(&whole)),
        "the fixture's best sum is some leg's best: {:?} vs {alone:?}",
        first(&whole)
    );

    // Then the best rows are shadowed by unfolded writes of the same ids, which no longer
    // match: a share cut at `top_k`, not `top_k + |shadow|`, loses the rows that replace them.
    let shadowed: Vec<Document> = whole
        .ids
        .iter()
        .take(3)
        .flatten()
        .map(|id| {
            let i: u32 = id.trim_start_matches('d').parse().unwrap();
            let mut d = tdoc(i, true);
            d.attrs
                .insert("text".to_owned(), Value::Str("zeta".to_owned()));
            d
        })
        .collect();
    let filtered = Predicate::Cmp("n".to_owned(), Op::Gt, Value::Int(20));
    let cases: Vec<(&str, Option<Predicate>, usize, u8, bool)> = vec![
        ("sum", None, 5, 0, false),
        ("sum, filtered", Some(filtered.clone()), 5, 0, false),
        ("top_k past every match", None, 500, 0, false),
        ("sum, phase 2 failing", None, 5, 2, false),
        ("sum, shadowed", None, 5, 0, true),
        ("sum, shadowed, phase 2 failing", None, 5, 2, true),
    ];
    let mut wrote = false;
    for (name, filter, top_k, fails, shadow) in cases {
        if shadow && !wrote {
            w.write("idx", shadowed.clone()).await.unwrap();
            wrote = true;
        }
        let unsplit = w
            .query_filtered_as("idx", &legs, filter.as_ref(), sum, top_k, eventual)
            .await
            .unwrap();
        let mut peers = peers_of(&store, filter.clone());
        peers.fails = fails;
        let split = w
            .query_split_as(
                "idx",
                &legs,
                filter.as_ref(),
                sum,
                top_k,
                eventual,
                Some(&peers),
            )
            .await
            .unwrap();
        assert_eq!(exactly(&split), exactly(&unsplit), "{name}");
        let opens = peers.opens.load(Ordering::SeqCst);
        assert!(opens >= 1, "{name}: not split");
        assert_eq!(peers.parts.load(Ordering::SeqCst), 0, "{name}");
        // Every share carried the cut, and none sent back more rows than it keeps.
        let keep = top_k + if shadow { 3 } else { 0 };
        assert_eq!(
            peers.cuts.lock().unwrap().clone(),
            vec![Some(keep); usize::try_from(opens).unwrap()],
            "{name}: the cut each share carried"
        );
        let most = peers.most_rows.load(Ordering::SeqCst);
        assert!(
            most <= keep as u64,
            "{name}: a share sent {most} rows, past {keep}"
        );
        if fails == 0 && top_k == 5 {
            // Measured, not vacuous: each share matched far more rows than it kept.
            assert!(most > 0, "{name}: no share sent anything back");
        }
        if name == "sum" {
            // Code review: what the fixture rests on lies in a peer's share -- the best sum,
            // and at least one of the rows the later cases shadow.
            let given = peers.given.lock().unwrap().clone();
            assert!(
                given.contains(&whole.hits[0].segment),
                "the best sum is the coordinator's own"
            );
            assert!(
                whole
                    .hits
                    .iter()
                    .take(3)
                    .any(|h| given.contains(&h.segment)),
                "every shadowed row is the coordinator's own"
            );
        }
    }
}
