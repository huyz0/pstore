#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

//! What storing each row's id beside the codes would cost a dense query — M42, BACKLOG row 26.
//!
//! A cold query is four sequential rounds: HEAD, the open, the legs, and the row blocks that
//! carry each hit's id. Row 26's alternative stores the id beside the codes a dense leg already
//! reads, so an id-only query finishes in three — at the price of an id per **candidate** (an
//! index row whose code the leg reads) rather than per **hit**. This prints that price.
//!
//! ⚠️ Runs **outside `cargo test`**, for the reason `scripts/recall.sh` gives: a 100,000-row
//! corpus inside the suite would be rebuilt once per mutant.
//!
//! ⚠️ **Nothing here is estimated except the ids themselves.** Every request is logged with its
//! round, and the log must agree with `DepthCounting`'s depth and `Accounted`'s requests and
//! bytes or the run panics. The ids estimate is `(id bytes + a 1-byte length prefix)` per code
//! row **actually fetched**: at gap 0 that is the candidates; at the production backend's
//! 1 MiB coalesce gap it is every code row the coalesced fetches carried (M6b), read back from
//! `Accounted`'s recorded ranges. ⚠️ So the 1 MiB ids figure is an **upper bound** (code
//! review): ids at 37 bytes a row would bridge fewer rows within the same gap than codes at 24.
//!
//! ⚠️ `provisional`: an in-memory store counts bytes and requests exactly and says nothing of
//! latency.
//!
//!   cargo run --release -p pstore-engine --example id_round

use bytes::Bytes;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, OpClass,
    Precondition, PutOutcome, TenantView,
};
use pstore_engine::Engine;
use pstore_format::{Document, Section, Segment};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{CasTag, LaneId, TenantId};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const DIM: usize = 128;
const CLUSTERS: usize = 256;
const QUERIES: usize = 20;
const TOP_K: usize = 10;
/// The assumed encoding: the id's bytes and a 1-byte length prefix.
const PREFIX: u64 = 1;
/// The production backend's declared gap (`ObjectStoreBackend::unprobed`).
const GAPS: [(&str, u64); 2] = [("0", 0), ("1MiB", 1024 * 1024)];
const PROBES: [usize; 3] = [2, 8, 32];

// ⚠️ Pinned from the run that produced M42's ledger: integer totals over `QUERIES` queries at
// 100,000 rows, p = 8, gap 0. Drift is a failure, not a comparison by eye.
const PIN_CANDIDATES: u64 = 688_215;
const PIN_TOTAL_BYTES: u64 = 112_243_752;
const PIN_ROUND4_BYTES: u64 = 1_678_932;
const PIN_DEPTH: usize = 4;

/// ⚠️ `DepthCounting` sits **above** `Accounted`, not beneath it. While `Accounted` records
/// ranges it resolves a suffix read's absolute span with a `head` of its own (unbilled, and only
/// while recording); beneath it, `DepthCounting` counted that bookkeeping `head` as a fifth
/// round the query never issued.
type Inner = DepthCounting<TenantView<MemoryStore>>;

/// One request as the harness saw it.
#[derive(Debug, Clone)]
struct Ev {
    round: usize,
    key: String,
    range: Range<u64>,
    bytes: u64,
}

/// The outermost store: logs every request with its round, and declares the coalesce gap the
/// query path plans its fetches with. Everything it sees is a fetch that reaches `Accounted`.
struct Log {
    inner: Inner,
    caps: Vec<Capabilities>,
    gap: AtomicUsize,
    in_flight: Arc<AtomicUsize>,
    depth: AtomicUsize,
    on: AtomicBool,
    evs: Mutex<Vec<Ev>>,
}

/// Held for the life of one request.
struct Guard(Arc<AtomicUsize>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Log {
    /// `DepthCounting`'s rule, kept independently: a round begins when a request starts while
    /// nothing is in flight.
    ///
    /// ⚠️ **No yield here**, unlike `DepthCounting`'s: the counter beneath yields, and a second
    /// yield would open a window where this layer holds a request the one beneath has not seen
    /// yet -- so the two would split rounds differently and disagree for the wrong reason.
    fn begin(&self) -> (Guard, usize) {
        if self.in_flight.fetch_add(1, Ordering::SeqCst) == 0 {
            self.depth.fetch_add(1, Ordering::SeqCst);
        }
        let round = self.depth.load(Ordering::SeqCst);
        (Guard(Arc::clone(&self.in_flight)), round)
    }

    fn rec(&self, round: usize, key: &Key, range: Range<u64>, bytes: u64) {
        if self.on.load(Ordering::SeqCst) {
            self.evs.lock().unwrap().push(Ev {
                round,
                key: key.as_str().to_owned(),
                range,
                bytes,
            });
        }
    }

    /// Records a read whether or not it succeeded: `Accounted` bills a refused request too.
    fn done(
        &self,
        round: usize,
        key: &Key,
        range: Option<Range<u64>>,
        out: &Result<Bytes, BlobError>,
    ) {
        let bytes = out.as_ref().map_or(0, |b| b.len() as u64);
        self.rec(round, key, range.unwrap_or(0..bytes), bytes);
    }

    fn start(&self) {
        self.depth.store(0, Ordering::SeqCst);
        self.evs.lock().unwrap().clear();
        self.on.store(true, Ordering::SeqCst);
    }

    fn stop(&self) -> Vec<Ev> {
        self.on.store(false, Ordering::SeqCst);
        std::mem::take(&mut *self.evs.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl BlobStore for Log {
    fn capabilities(&self) -> &Capabilities {
        &self.caps[self.gap.load(Ordering::SeqCst)]
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        let (_g, r) = self.begin();
        let out = self.inner.get(key).await;
        self.done(r, key, None, &out);
        out
    }

    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        let (_g, r) = self.begin();
        let out = self.inner.get_range(key, range.clone()).await;
        self.done(r, key, Some(range), &out);
        out
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        let (_g, r) = self.begin();
        let out = self.inner.get_range_as(key, range.clone(), class).await;
        self.done(r, key, Some(range), &out);
        out
    }

    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        let (_g, r) = self.begin();
        let out = self.inner.get_immutable(key, class).await;
        self.done(r, key, None, &out);
        out
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        let (_g, r) = self.begin();
        let out = self.inner.get_suffix(key, n).await;
        self.done(r, key, Some(0..n), &out);
        out
    }

    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        let (_g, r) = self.begin();
        let out = self.inner.get_suffix_as(key, n, class).await;
        self.done(r, key, Some(0..n), &out);
        out
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        let (_g, r) = self.begin();
        let out = self.inner.get_with_tag(key).await;
        let bytes = out.as_ref().map_or(0, |(b, _)| b.len() as u64);
        self.rec(r, key, 0..bytes, bytes);
        out
    }

    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        let (_g, r) = self.begin();
        let t = self.inner.get_tag(key).await;
        self.rec(r, key, 0..0, 0);
        t
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        let (_g, r) = self.begin();
        let t = self.inner.head(key).await;
        self.rec(r, key, 0..0, 0);
        t
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        let (_g, _) = self.begin();
        self.inner.put(key, body).await
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        let (_g, _) = self.begin();
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        let (_g, _) = self.begin();
        self.inner.delete_batch(keys).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        panic!("a query LISTed {prefix:?}")
    }
}

/// xorshift64: deterministic, so every run measures the same corpus.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unif(&mut self) -> f32 {
        (self.next() % 1_000_000) as f32 / 1_000_000.0
    }

    fn gauss(&mut self) -> f32 {
        let u1 = self.unif().max(1e-6);
        let u2 = self.unif();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }

    /// A 36-byte UUID-shaped id.
    fn id(&mut self) -> String {
        let h = format!("{:016x}{:016x}", self.next(), self.next());
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    }
}

fn near(rng: &mut Rng, centres: &[Vec<f32>]) -> Vec<f32> {
    let c = &centres[(rng.next() % centres.len() as u64) as usize];
    let mut v: Vec<f32> = c.iter().map(|x| x + 0.08 * rng.gauss()).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    for x in &mut v {
        *x /= n;
    }
    v
}

fn overlap(r: &Range<u64>, s: &Range<u64>) -> u64 {
    r.end.min(s.end).saturating_sub(r.start.max(s.start))
}

/// Integer totals over `QUERIES` queries of one configuration.
#[derive(Default)]
struct Totals {
    candidates: u64,
    leg_req: u64,
    leg_bytes: u64,
    r4_req: u64,
    r4_bytes: u64,
    total_req: u64,
    total_bytes: u64,
    /// Code rows the leg round's fetches carried, read from `Accounted`'s recorded ranges.
    rows_fetched: u64,
}

/// What a scale's segment looks like, for attributing a fetch to its section.
struct Layout {
    key: Key,
    /// The section a leg reads per candidate: RaBitQ when clustered, Vectors when exact.
    codes: Range<u64>,
    code_len: u64,
    exact: bool,
}

struct Scale {
    log: Arc<Log>,
    depth: Inner,
    acct: Accounted<MemoryStore>,
    tenant: TenantId,
    params: pstore_index::cluster::Params,
    layout: Layout,
    queries: Vec<Vec<f32>>,
    id_len: u64,
}

async fn build(rows: usize) -> Scale {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ rows as u64);
    let centres: Vec<Vec<f32>> = (0..CLUSTERS)
        .map(|_| {
            let v: Vec<f32> = (0..DIM).map(|_| rng.gauss()).collect();
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
            v.into_iter().map(|x| x / n).collect()
        })
        .collect();
    let docs: Vec<Document> = (0..rows)
        .map(|_| {
            let v = near(&mut rng, &centres);
            Document::new(rng.id(), v)
        })
        .collect();
    let id_bytes: usize = docs.iter().map(|d| d.id.len()).sum();
    assert_eq!(id_bytes, rows * 36, "every id is 36 bytes");

    let acct = Accounted::new(MemoryStore::new());
    let tenant = TenantId(7);
    let depth = DepthCounting::new(acct.as_tenant(tenant));
    let base = depth.capabilities().clone();
    let log = Arc::new(Log {
        inner: depth.clone(),
        caps: GAPS
            .iter()
            .map(|(_, g)| Capabilities {
                coalesce_gap: *g,
                ..base.clone()
            })
            .collect(),
        gap: AtomicUsize::new(0),
        in_flight: Arc::new(AtomicUsize::new(0)),
        depth: AtomicUsize::new(0),
        on: AtomicBool::new(false),
        evs: Mutex::new(Vec::new()),
    });
    // `Engine::new`'s own defaults (`replicas: 0`, 4,000-row lists, the 25,000-row threshold),
    // restated because they are private: 20,000 rows is the exact path and the larger scales
    // are clustered.
    let params = pstore_index::cluster::Params {
        replicas: 0,
        boundary: 0.0,
        ..pstore_index::cluster::Params::default()
    };
    let writer = Engine::new(Arc::clone(&log), tenant, LaneId(0)).with_index_params(params);
    for chunk in docs.chunks(5_000) {
        writer.write("idx", chunk.to_vec()).await.unwrap();
        writer.flush().await.unwrap();
    }
    writer.fold().await.unwrap();

    let head = writer.head_for_test().await;
    let refs = &head.indexes["idx"];
    assert_eq!(refs.len(), 1, "the fold must leave one segment");
    let key = Key::new(refs[0].key.clone());
    let seg = Segment::open(&log.inner, &key).await.unwrap();
    let index_rows = seg.index_row_count() as u64;
    // ⚠️ Clustered is answered by whether the centroid object exists (D-10), not by the
    // sections: a segment below the threshold may still carry codes it is never searched by.
    let clustered = log
        .inner
        .get(&pstore_index::vec_index::centroid_key(&key))
        .await
        .is_ok();
    let layout = if clustered {
        let codes = seg
            .section(Section::RaBitQ)
            .expect("a clustered segment has codes");
        Layout {
            code_len: (codes.end - codes.start) / index_rows,
            codes,
            key,
            exact: false,
        }
    } else {
        let codes = seg.section(Section::Vectors).expect("a dense segment");
        Layout {
            code_len: (codes.end - codes.start) / seg.row_count() as u64,
            codes,
            key,
            exact: true,
        }
    };
    let queries = (0..QUERIES).map(|_| near(&mut rng, &centres)).collect();
    Scale {
        log,
        depth,
        acct,
        tenant,
        params,
        layout,
        queries,
        id_len: 36,
    }
}

/// One cold query: a fresh engine, so nothing is cached; every request logged and checked.
async fn measure(s: &Scale, p: usize, gap: usize) -> Totals {
    s.log.gap.store(gap, Ordering::SeqCst);
    let mut t = Totals::default();
    for q in &s.queries {
        let engine =
            Engine::new(Arc::clone(&s.log), s.tenant, LaneId(1)).with_index_params(s.params);
        s.depth.reset();
        s.acct.record_ranges();
        let (req0, bytes0) = (
            s.acct.count(s.tenant, OpClass::Read),
            s.acct.bytes(s.tenant, OpClass::Read),
        );
        s.log.start();
        let answer = engine
            .query(
                "idx",
                &[pstore_query::Prefetch::Dense {
                    field: pstore_format::DEFAULT_FIELD.to_owned(),
                    query: q.clone(),
                    limit: TOP_K,
                    tune: pstore_index::vec_index::Query {
                        p,
                        ..pstore_index::vec_index::Query::default()
                    },
                }],
                pstore_query::Fusion::Rrf { k: 60.0 },
                TOP_K,
            )
            .await
            .unwrap();
        let evs = s.log.stop();
        assert_eq!(answer.hits.len(), TOP_K);
        assert!(
            answer.ids.iter().all(Option::is_some),
            "a hit without its id"
        );

        // ⚠️ The self-check: the log is the only source of the per-round split, so it must
        // agree with the two counters that do not depend on it.
        let requests = s.acct.count(s.tenant, OpClass::Read) - req0;
        let bytes = s.acct.bytes(s.tenant, OpClass::Read) - bytes0;
        let rounds: std::collections::BTreeSet<usize> = evs.iter().map(|e| e.round).collect();
        let logged: u64 = evs.iter().map(|e| e.bytes).sum();
        assert_eq!(
            evs.len() as u64,
            requests,
            "the log holds {} requests and Accounted counted {requests}",
            evs.len()
        );
        assert_eq!(
            rounds.len(),
            s.depth.depth(),
            "the log spans {} rounds and DepthCounting measured {}",
            rounds.len(),
            s.depth.depth()
        );
        assert_eq!(
            logged, bytes,
            "the log holds {logged} bytes and Accounted counted {bytes}"
        );
        assert_eq!(
            s.depth.depth(),
            PIN_DEPTH,
            "a query took {} rounds",
            s.depth.depth()
        );

        let l = &s.layout;
        let leg = evs
            .iter()
            .find(|e| e.key == l.key.as_str() && overlap(&e.range, &l.codes) > 0)
            .map(|e| e.round)
            .expect("no request read the codes");
        assert_eq!(leg, 3, "the leg is the third round");
        // Code rows the leg's fetches carried, from `Accounted`'s own record of the ranges.
        let code_bytes = s.acct.bytes_in(&l.key, l.codes.clone());
        let in_round = |r: usize| evs.iter().filter(move |e| e.round == r);
        let leg_code_bytes: u64 = in_round(leg)
            .filter(|e| e.key == l.key.as_str())
            .map(|e| overlap(&e.range, &l.codes))
            .sum();
        assert_eq!(code_bytes, leg_code_bytes, "only the leg reads codes");
        let fetched = code_bytes / l.code_len;
        if gap == 0 {
            // At gap 0 nothing is fetched that was not asked for: the rows are the candidates.
            t.candidates += fetched;
        }
        t.rows_fetched += fetched;
        t.leg_req += in_round(leg).count() as u64;
        t.leg_bytes += in_round(leg).map(|e| e.bytes).sum::<u64>();
        t.r4_req += in_round(4).count() as u64;
        t.r4_bytes += in_round(4).map(|e| e.bytes).sum::<u64>();
        t.total_req += requests;
        t.total_bytes += bytes;
    }
    t
}

fn row(rows: usize, s: &Scale, gap: usize, p: Option<usize>, t: &Totals, candidates: u64) {
    let n = QUERIES as f64;
    let per_row = s.id_len + PREFIX;
    let ids = t.rows_fetched * per_row;
    let pct = |x: f64| 100.0 * x / t.total_bytes as f64;
    println!(
        "| {rows} | {} | {} | {} | {:.0} | {:.1} | {:.0} | {:.1} | {:.0} | {:.1} | {:.0} | {:.0} | {:.0} ({:+.1}%) | {:+.1}% |",
        if s.layout.exact { "exact" } else { "ivf" },
        GAPS[gap].0,
        p.map_or_else(|| "-".to_owned(), |p| p.to_string()),
        candidates as f64 / n,
        t.leg_req as f64 / n,
        t.leg_bytes as f64 / n,
        t.r4_req as f64 / n,
        t.r4_bytes as f64 / n,
        t.total_req as f64 / n,
        t.total_bytes as f64 / n,
        t.rows_fetched as f64 / n,
        ids as f64 / n,
        pct(ids as f64),
        pct(ids as f64 - t.r4_bytes as f64),
    );
}

async fn run() {
    println!(
        "M42: cold dense query, top_k {TOP_K}, mean of {QUERIES} queries; {DIM}-d Gaussian \
         mixture ({CLUSTERS} centres), 36-byte ids, ids estimate = (36 + {PREFIX}) B per code \
         row fetched. provisional: in-memory store.\n"
    );
    println!(
        "| rows | path | gap | p | candidates | leg req | leg B | round-4 req | round-4 B | total req | total B | code rows fetched | +ids beside codes B | net after dropping round 4 |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let mut pinned = None;
    for rows in [20_000, 100_000, 400_000] {
        let s = build(rows).await;
        let probes: Vec<Option<usize>> = if s.layout.exact {
            vec![None]
        } else {
            PROBES.iter().copied().map(Some).collect()
        };
        assert_eq!(
            s.layout.exact,
            rows < 25_000,
            "the path follows D-10's threshold"
        );
        for p in probes {
            let width = p.unwrap_or(pstore_index::vec_index::Query::default().p);
            let exact = measure(&s, width, 0).await;
            let candidates = exact.candidates;
            row(rows, &s, 0, p, &exact, candidates);
            let wide = measure(&s, width, 1).await;
            row(rows, &s, 1, p, &wide, candidates);
            if rows == 100_000 && p == Some(8) {
                pinned = Some((exact.candidates, exact.total_bytes, exact.r4_bytes));
            }
        }
    }
    let (candidates, total, r4) = pinned.expect("the pinned configuration ran");
    println!(
        "\npinned (100,000 rows, p = 8, gap 0, totals over {QUERIES} queries): candidates \
         {candidates}, total bytes {total}, round-4 bytes {r4}, depth {PIN_DEPTH} per query"
    );
    assert_eq!(
        (candidates, total, r4),
        (PIN_CANDIDATES, PIN_TOTAL_BYTES, PIN_ROUND4_BYTES),
        "the 100,000-row, p = 8 constants drifted"
    );
    println!("ok: the log agrees with DepthCounting and Accounted, and the pins hold");
}

fn main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(run());
}
