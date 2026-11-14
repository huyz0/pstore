#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

//! The scalar-storage benchmark (M230 onward): what a filter costs a segment, at gate scale.
//!
//! The milestones of `docs/comparisons/column-storage-adoption.md` each change what a filtered
//! read fetches or decodes. This harness measures the same corpus and the same predicates
//! before and after each, so a milestone's `VERIFIED.md` records a measurement rather than the
//! comparison doc's arithmetic.
//!
//! What it reports, per predicate: blocks kept of the segment's blocks, GETs and bytes read
//! (counted by `Accounted`, so the opening suffix read is excluded), rows admitted, and the
//! median wall time of the mask (the same fetch, projection and `admits` that
//! `pstore-query`'s `mask` runs) over `RUNS` runs against the in-memory store. Per segment:
//! its bytes, its meta region's bytes, and its blocks.
//!
//! ⚠️ **Every time it prints is `provisional`**: an in-memory store, one container, no network.
//! Times are relative, between arms of one run, never absolute. Bytes and counts are exact.
//!
//!   scripts/columns.sh              20,000 rows
//!   scripts/columns.sh 100000       another scale

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{BlockOptions, Document, Projection, Segment, SegmentWriter, Value};
use pstore_query::{Aggregate, AggregateSpec, Aggregator, Op, Predicate, Target};
use pstore_types::TenantId;
use std::time::Instant;

const RUNS: usize = 31;
const CATEGORIES: usize = 40;
/// Rows a customer's batch holds: rows arrive customer by customer, as a multi-customer
/// tenant's ingest often does, so a customer's rows sit in a few neighbouring blocks.
const BATCH: usize = 40;

/// A deterministic xorshift, so every arm sees the same corpus.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The corpus: per row an id, a small vector, and the attributes a tenant filter names.
///
/// - `category`: one of 40 strings, uniform -- every block holds every value, so pruning can
///   do nothing and the arm measures only its cost.
/// - `customer`: a string, the same for `BATCH` consecutive rows -- selective and clustered.
/// - `sku`: a string unique to the row, in random order -- selective and unclustered.
/// - `archived`: a bool, true for the oldest tenth of rows -- clustered.
/// - `promo`: a bool, true for one row in a hundred, at random.
/// - `price`: an int, uniform over 1..=10,000. `rating`: a float. `ts`: a datetime, ascending.
/// - `title`: a 40-character string, the bulk a row carries besides its filter columns.
fn corpus(rows: usize) -> Vec<Document> {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    (0..rows)
        .map(|i| {
            let mut d = Document::new(format!("d{i:07}"), vec![1.0, 0.0]);
            let a = &mut d.attrs;
            let cat = (rng.next() % CATEGORIES as u64) as usize;
            a.insert("category".into(), Value::Str(format!("cat-{cat:02}")));
            a.insert(
                "customer".into(),
                Value::Str(format!("cust-{:05}", i / BATCH)),
            );
            a.insert("sku".into(), Value::Str(format!("sku-{:016x}", rng.next())));
            a.insert("archived".into(), Value::Bool(i < rows / 10));
            a.insert("promo".into(), Value::Bool(rng.next().is_multiple_of(100)));
            a.insert("price".into(), Value::Int((rng.next() % 10_000) as i64 + 1));
            a.insert(
                "rating".into(),
                Value::Float((rng.next() % 500) as f64 / 100.0),
            );
            a.insert(
                "ts".into(),
                Value::DateTime(1_700_000_000_000_000 + i as i64 * 1_000_000),
            );
            let title: String = (0..40)
                .map(|_| char::from(b'a' + (rng.next() % 26) as u8))
                .collect();
            a.insert("title".into(), Value::Str(title));
            d
        })
        .collect()
}

fn eq(name: &str, v: Value) -> Predicate {
    Predicate::Cmp(name.to_owned(), Op::Eq, v)
}

/// The predicates measured, by label. Each is a filter a tenant writes.
fn predicates(rows: usize, docs: &[Document]) -> Vec<(&'static str, Predicate)> {
    let sku = docs[rows / 2].attrs["sku"].clone();
    vec![
        (
            "category = cat-07 (uniform)",
            eq("category", Value::Str("cat-07".into())),
        ),
        (
            "customer = one batch (clustered)",
            eq(
                "customer",
                Value::Str(format!("cust-{:05}", rows / BATCH / 3)),
            ),
        ),
        (
            "customer in 3 batches",
            Predicate::In(
                "customer".into(),
                (0..3)
                    .map(|k| Value::Str(format!("cust-{:05}", k * rows / BATCH / 4)))
                    .collect(),
            ),
        ),
        ("sku = one row (unclustered)", eq("sku", sku)),
        (
            "customer = absent value",
            eq("customer", Value::Str("nobody".into())),
        ),
        (
            "archived = true (clustered)",
            eq("archived", Value::Bool(true)),
        ),
        ("promo = true (1%, random)", eq("promo", Value::Bool(true))),
        (
            "price < 100 (zone map, control)",
            Predicate::Cmp("price".into(), Op::Lt, Value::Int(100)),
        ),
        (
            "customer = one batch AND price < 5000",
            Predicate::And(vec![
                eq(
                    "customer",
                    Value::Str(format!("cust-{:05}", rows / BATCH / 3)),
                ),
                Predicate::Cmp("price".into(), Op::Lt, Value::Int(5000)),
            ]),
        ),
    ]
}

/// What `pstore-query`'s `mask` does, through the public API: the zones decide the blocks,
/// one coalesced fetch reads them, each row is built only as far as the predicate reads it.
async fn mask<S: BlobStore>(store: &S, key: &Key, seg: &Segment, p: &Predicate) -> usize {
    let names = names(p);
    let wanted = |name: &str| names.binary_search(&name).is_ok();
    let project = Projection {
        id: false,
        attrs: &wanted,
    };
    let mut admitted = 0usize;
    seg.visit_rows_projected(
        store,
        key,
        |z| p.could_admit(z),
        &project,
        |_, d| {
            if p.admits(&d.id, &d.attrs) {
                admitted += 1;
            }
        },
    )
    .await
    .expect("mask");
    admitted
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let rows: usize = std::env::args()
        .nth(1)
        .map_or(20_000, |s| s.parse().expect("a row count"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(run(rows));
}

/// A layout under test: its label and how its writer is configured.
type Arm = (&'static str, fn(SegmentWriter) -> SegmentWriter);

/// The arms, oldest layout first, so a milestone's gain is the step between two rows.
const ARMS: &[Arm] = &[
    ("M229 (row blocks, no equality section)", |w| {
        w.with_block_options(BlockOptions {
            columnar: false,
            equality: false,
            aggregates: false,
        })
    }),
    ("M230 (row blocks, equality section)", |w| {
        w.with_block_options(BlockOptions {
            columnar: false,
            equality: true,
            aggregates: false,
        })
    }),
    ("M231 (columnar blocks)", |w| {
        w.with_block_options(BlockOptions {
            aggregates: false,
            ..BlockOptions::default()
        })
    }),
    ("M232 (columnar blocks, aggregate statistics)", |w| w),
];

/// The aggregations M232 measures: what each costs with and without block statistics.
fn aggregations() -> Vec<(&'static str, AggregateSpec, Option<Predicate>)> {
    let spec = |label: &str, agg: Aggregate| AggregateSpec {
        labels: vec![(label.to_owned(), agg)],
        group_by: vec![],
        top_k: 10,
    };
    vec![
        (
            "sum(price)",
            spec("s", Aggregate::Sum("price".into())),
            None,
        ),
        (
            "avg(rating) where price >= 1 (all)",
            spec("a", Aggregate::Avg("rating".into())),
            Some(Predicate::Cmp("price".into(), Op::Gte, Value::Int(1))),
        ),
        (
            "max(price) where price >= 5001 (half)",
            spec("m", Aggregate::Max("price".into())),
            Some(Predicate::Cmp("price".into(), Op::Gte, Value::Int(5001))),
        ),
        (
            "count where category = (no stats)",
            spec("c", Aggregate::Count(None)),
            Some(eq("category", Value::Str("cat-07".into()))),
        ),
    ]
}

async fn run(rows: usize) {
    let docs = corpus(rows);
    println!("# columns bench, {rows} rows. ⚠️ provisional: in-memory store, one container.");
    for (arm, configure) in ARMS {
        // The object-store backend's coalescing gap, so GETs count what S3 would be sent.
        let acct = Accounted::new(MemoryStore::with_coalesce_gap(1024 * 1024));
        let tenant = TenantId(1);
        let view = acct.as_tenant(tenant);
        let key = Key::new("bench/columns.seg");

        let t = Instant::now();
        let mut w = configure(SegmentWriter::new(64));
        for d in &docs {
            w.push(d.clone());
        }
        let bytes = w.try_finish().expect("segment");
        let write_ms = t.elapsed().as_secs_f64() * 1e3;
        let seg_len = bytes.len();
        let meta_len = pstore_format::index_section_len(&bytes).unwrap_or(0);
        view.put(&key, bytes).await.expect("put");
        let seg = Segment::open(&view, &key).await.expect("open");
        let blocks = seg.block_count();
        // M230: the equality section's shape, when the segment has one.
        let shape = seg
            .block_zones()
            .next()
            .and_then(|z| z.equality.as_ref())
            .map(|(e, _)| e.shape());

        println!();
        println!("## {arm}");
        println!(
            "segment {seg_len} B, meta region {meta_len} B, {blocks} blocks of ~{} rows, \
             write {write_ms:.0} ms",
            rows / blocks.max(1)
        );
        match shape {
            Some((bits, strings, bools)) => {
                println!("equality section: {bits}-bit filters for {strings:?}, bools {bools:?}");
            }
            None => println!("equality section: none"),
        }
        // Whole-segment decode, and resolving 10 hits spread over the segment (`rows_at`).
        let mut full = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let t = Instant::now();
            let n = seg
                .rows_where(&view, &key, |_| true)
                .await
                .expect("scan")
                .len();
            full.push(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(n, rows);
        }
        let hits: Vec<usize> = (0..10).map(|k| k * rows / 10 + 3).collect();
        let mut resolve = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let t = Instant::now();
            let got = seg.rows_at(&view, &key, &hits).await.expect("resolve");
            resolve.push(t.elapsed().as_secs_f64() * 1e3);
            assert!(got.iter().all(Option::is_some));
        }
        println!(
            "decode every row {:.2} ms, resolve 10 hits {:.3} ms",
            median(full),
            median(resolve)
        );
        println!(
            "{:<40} {:>9} {:>5} {:>11} {:>8} {:>10}",
            "predicate", "blocks", "GETs", "bytes", "rows", "mask ms"
        );
        for (label, p) in predicates(rows, &docs) {
            let kept = seg.block_zones().filter(|z| p.could_admit(z)).count();
            let gets0 = acct.count(tenant, OpClass::Read);
            let bytes0 = acct.bytes(tenant, OpClass::Read);
            let admitted = mask(&view, &key, &seg, &p).await;
            let gets = acct.count(tenant, OpClass::Read) - gets0;
            let moved = acct.bytes(tenant, OpClass::Read) - bytes0;
            let mut times = Vec::with_capacity(RUNS);
            for _ in 0..RUNS {
                let t = Instant::now();
                let n = mask(&view, &key, &seg, &p).await;
                times.push(t.elapsed().as_secs_f64() * 1e3);
                assert_eq!(n, admitted, "a run admitted other rows");
            }
            let want = docs.iter().filter(|d| p.admits(&d.id, &d.attrs)).count();
            assert_eq!(admitted, want, "{label}: the mask lost or invented rows");
            println!(
                "{label:<40} {kept:>4}/{blocks:<4} {gets:>5} {moved:>11} {admitted:>8} {:>10.3}",
                median(times)
            );
        }
        // M232: aggregations, through the query path every server runs.
        let target = Target {
            segment: key.clone(),
            segment_len: Some(seg_len as u64),
            centroids: None,
            deleted: None,
            sparse_dict: false,
            text_dict: false,
            fde_clusters: false,
            shadowed: false,
        };
        let none = std::collections::HashSet::new();
        println!(
            "{:<40} {:>5} {:>11} {:>10}  answer",
            "aggregation", "GETs", "bytes", "ms"
        );
        for (label, spec, filter) in aggregations() {
            let run = || async {
                let mut a = Aggregator::new(spec.clone());
                pstore_query::aggregate(
                    &view,
                    std::slice::from_ref(&target),
                    filter.as_ref(),
                    &none,
                    &mut a,
                )
                .await
                .expect("aggregate");
                a.finish()
            };
            let gets0 = acct.count(tenant, OpClass::Read);
            let bytes0 = acct.bytes(tenant, OpClass::Read);
            let answer = run().await;
            let gets = acct.count(tenant, OpClass::Read) - gets0;
            let moved = acct.bytes(tenant, OpClass::Read) - bytes0;
            let mut times = Vec::with_capacity(RUNS);
            for _ in 0..RUNS {
                let t = Instant::now();
                assert_eq!(run().await, answer, "a run answered otherwise");
                times.push(t.elapsed().as_secs_f64() * 1e3);
            }
            println!(
                "{label:<40} {gets:>5} {moved:>11} {:>10.3}  {:?}",
                median(times),
                answer[0].1
            );
        }
    }
}

/// The attributes a predicate reads, sorted: what `mask` projects.
fn names(p: &Predicate) -> Vec<&str> {
    let mut out = Vec::new();
    names_into(p, &mut out);
    out.sort_unstable();
    out.dedup();
    out
}

fn names_into<'a>(p: &'a Predicate, out: &mut Vec<&'a str>) {
    match p {
        Predicate::Cmp(n, ..) | Predicate::In(n, _) => out.push(n),
        Predicate::And(ps) | Predicate::Or(ps) => ps.iter().for_each(|q| names_into(q, out)),
        Predicate::Not(q) => names_into(q, out),
        _ => unreachable!("the bench writes no other predicate"),
    }
}
